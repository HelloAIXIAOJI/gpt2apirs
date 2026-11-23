//! 后台维护任务。
//!
//!
//! 每轮四类处理：限流号重查配额、陈旧号重查、AT 距过期不足 24h 则刷新、
//! 超过 3 天没刷新过的走保活刷新（受批量上限约束，避免一轮打满上游）。

use std::sync::Arc;
use std::time::Duration;

use chrono::{Utc};
use gpt2apirs_core::Deadline;

use crate::consts::{
    ACCESS_REFRESH_SKEW_SECS, KEEPALIVE_BATCH, KEEPALIVE_INTERVAL_SECS, KEEPALIVE_MAX_PER_ROUND,
};
use crate::pool::AccountPool;
use crate::progress::ProgressStore;
use crate::sync::{needs_sync, needs_token_refresh, run_refresh_batch, run_sync_batch};

/// watcher 配置。
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// 轮询间隔（秒），对应 `refresh_account_interval_minute`。
    pub interval_secs: u64,
    /// 单轮内并发数。
    pub concurrency: usize,
    /// 单轮最多同步多少个账号（防止一轮打太久）。
    pub max_sync_per_round: usize,
    /// 单轮最多刷新多少个 token。
    pub max_refresh_per_round: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            // 默认 5 分钟一轮
            interval_secs: 300,
            concurrency: 4,
            max_sync_per_round: 30,
            max_refresh_per_round: KEEPALIVE_MAX_PER_ROUND,
        }
    }
}

/// 一轮维护的统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatcherRound {
    /// 快照账号数。
    pub accounts: usize,
    /// 本轮触发的同步账号数。
    pub synced: usize,
    /// 本轮触发的 token 刷新数。
    pub refreshed: usize,
}

/// 跑一轮维护。返回本轮做了什么。
pub async fn run_round(
    pool: &Arc<AccountPool>,
    progress: &Arc<ProgressStore>,
    config: &WatcherConfig,
) -> WatcherRound {
    let accounts = match pool.reload().await {
        Ok(_) => pool.snapshot().await,
        Err(error) => {
            tracing::warn!(error = %error, "账号池快照重载失败，跳过本轮");
            return WatcherRound::default();
        }
    };

    let now = Utc::now();
    let mut refresh_ids = Vec::new();
    let mut sync_ids = Vec::new();

    for account in accounts.iter() {
        // token 刷新优先：AT 都快过期了，先续期再谈配额
        if needs_token_refresh(account, now) && refresh_ids.len() < config.max_refresh_per_round {
            refresh_ids.push(account.id);
            continue;
        }
        // keepalive：长期没刷新过的 refresh_token 也可能失效
        if keepalive_due(account, now) && refresh_ids.len() < config.max_refresh_per_round {
            refresh_ids.push(account.id);
            continue;
        }
        if needs_sync(account, now) && sync_ids.len() < config.max_sync_per_round {
            sync_ids.push(account.id);
        }
    }

    let deadline = Deadline::in_secs(600.0);
    let mut round = WatcherRound {
        accounts: accounts.len(),
        ..Default::default()
    };

    if !sync_ids.is_empty() {
        round.synced = sync_ids.len();
        run_sync_batch(pool, progress, sync_ids, deadline, config.concurrency).await;
    }
    if !refresh_ids.is_empty() {
        round.refreshed = refresh_ids.len();
        run_refresh_batch(pool, progress, refresh_ids, deadline, config.concurrency).await;
    }

    progress.prune();
    round
}

fn keepalive_due(account: &crate::model::Account, now: chrono::DateTime<Utc>) -> bool {
    if account
        .refresh_token
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return false;
    }
    match account.last_refresh_at {
        None => true,
        Some(last) => (now - last).num_seconds() > KEEPALIVE_INTERVAL_SECS,
    }
}

/// 启动后台 watcher。返回句柄，drop 前一直运行。
pub fn spawn_watcher(
    pool: Arc<AccountPool>,
    progress: Arc<ProgressStore>,
    config: WatcherConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(config.interval_secs.max(1));
        // 启动时先跑一轮，避免导入后要等一个周期才生效
        let round = run_round(&pool, &progress, &config).await;
        tracing::info!(
            accounts = round.accounts,
            synced = round.synced,
            refreshed = round.refreshed,
            "账号池首轮维护完成"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // 第一次 tick 立即返回，跳过
        loop {
            ticker.tick().await;
            let round = run_round(&pool, &progress, &config).await;
            tracing::debug!(
                accounts = round.accounts,
                synced = round.synced,
                refreshed = round.refreshed,
                "账号池维护轮次完成"
            );
        }
    })
}

/// keepalive 与刷新相关的常量再导出，便于测试断言。
pub const fn refresh_skew_secs() -> i64 {
    ACCESS_REFRESH_SKEW_SECS
}

/// 批量 keepalive 大小。
pub const fn keepalive_batch() -> usize {
    KEEPALIVE_BATCH
}

/// keepalive 间隔。
pub const fn keepalive_interval_secs() -> i64 {
    KEEPALIVE_INTERVAL_SECS
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;
    use super::*;
    use crate::model::{Account, AccountStatus, Plan, Source};
    use base64::Engine as _;
    use uuid::Uuid;

    fn jwt(exp_offset_secs: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            format!(r#"{{"exp":{}}}"#, (Utc::now() + ChronoDuration::seconds(exp_offset_secs)).timestamp())
                .as_bytes(),
        );
        format!("{header}.{payload}.sig")
    }

    fn account(exp_offset: i64, last_refresh_days_ago: Option<i64>) -> Account {
        let now = Utc::now();
        Account {
            id: Uuid::new_v4(),
            access_token: jwt(exp_offset),
            refresh_token: Some("rt".to_string()),
            id_token: None,
            email: Some("w@x.com".to_string()),
            upstream_account_id: None,
            plan: Plan::Free,
            source: Source::Web,
            status: AccountStatus::Normal,
            quota: 1,
            quota_confirmed: true,
            quota_unlimited: false,
            restore_at: None,
            proxy: String::new(),
            success_count: 0,
            fail_count: 0,
            invalid_count: 0,
            last_used_at: None,
            last_invalid_at: None,
            last_error: None,
            last_error_at: None,
            last_checked_at: Some(now),
            last_refresh_at: last_refresh_days_ago
                .map(|days| now - ChronoDuration::days(days)),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn keepalive_due_for_never_refreshed() {
        assert!(keepalive_due(&account(86400 * 30, None), Utc::now()));
    }

    #[test]
    fn keepalive_due_after_three_days() {
        assert!(keepalive_due(&account(86400 * 30, Some(4)), Utc::now()));
        assert!(!keepalive_due(&account(86400 * 30, Some(1)), Utc::now()));
    }

    #[test]
    fn keepalive_not_due_without_refresh_token() {
        let mut account = account(86400 * 30, None);
        account.refresh_token = None;
        assert!(!keepalive_due(&account, Utc::now()));
    }

    #[test]
    fn constants_are_exposed_for_tests() {
        assert_eq!(refresh_skew_secs(), 86_400);
        assert_eq!(keepalive_interval_secs(), 259_200);
        assert_eq!(keepalive_batch(), 3);
    }

    #[test]
    fn default_config_matches_reference() {
        let config = WatcherConfig::default();
        assert_eq!(config.interval_secs, 300, "默认 5 分钟");
        assert!(config.concurrency >= 1);
        assert!(config.max_sync_per_round >= 1);
    }
}

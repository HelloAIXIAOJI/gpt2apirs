//! 同步（拉配额）与只刷 token 两个维护操作。
//!
//! 对照设计文档 6.1 的「三个刷新操作」：
//!
//! 两个操作分工：`sync` 打 `/me` 拿真实配额（顺带换 AT）；`refresh-at` 只换 AT，不查配额。

use std::sync::Arc;

use chrono::Utc;
use gpt2apirs_core::Deadline;
use uuid::Uuid;

use crate::model::{Account, AccountStatus};
use crate::pool::{AccountPool, PoolError};
use crate::progress::{OperationKind, ProgressStore};
use crate::refresh::{RefreshError, refresh_tokens};
use crate::remote::{RemoteError, fetch_remote_info};
use crate::repo::AccountPatch;

/// 单账号同步失败。
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// 仓储/池错误。
    #[error(transparent)]
    Pool(#[from] PoolError),

    /// 仓储错误。
    #[error("账号库操作失败：{0}")]
    Repo(#[from] crate::repo::AccountError),

    /// 上游读取失败。
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

/// 同步一个账号：拉账号资料 + 图片配额，写入库。
///
/// 上游明确说 token 无效时，会走失效处置流程。
pub async fn sync_account(
    pool: &AccountPool,
    account_id: Uuid,
    deadline: Deadline,
) -> Result<Account, SyncError> {
    let account = pool
        .repo()
        .get_by_id(account_id)
        .await?
        .ok_or_else(|| PoolError::NoCandidate {
            reason: format!("账号 {account_id} 不存在"),
        })?;

    let client = pool.client()?;
    let remote = match fetch_remote_info(
        client,
        &account.access_token,
        pool.user_agent(),
        deadline,
    )
    .await
    {
        Ok(info) => info,
        Err(error) if error.is_unauthorized() => {
            pool.handle_invalid(account_id, &error.to_string()).await?;
            return Err(SyncError::Remote(error));
        }
        Err(error) => {
            let _ = pool
                .repo()
                .record_invalid(account_id, &error.to_string())
                .await;
            return Err(SyncError::Remote(error));
        }
    };

    // 状态推导：禁用不动；配额为 0  限流；否则正常。
    let status = match account.status {
        AccountStatus::Disabled => AccountStatus::Disabled,
        _ if remote.quota == 0 => AccountStatus::Limited,
        _ => AccountStatus::Normal,
    };

    let updated = pool
        .repo()
        .patch(
            account_id,
            &AccountPatch {
                status: Some(status),
                plan: remote.plan,
                email: remote.email,
                quota: Some(remote.quota),
                quota_confirmed: Some(true),
                restore_at: Some(remote.restore_at),
                last_error: Some(None),
                ..Default::default()
            },
        )
        .await?;
    let updated = pool.repo().mark_checked(account_id).await.map(|_| updated)?;

    // 同步成功后清掉失效计数（远端已确认可用）
    let updated = pool.clear_invalid(account_id).await.unwrap_or(updated);
    Ok(updated)
}

/// 只刷新一个账号的 access token。
pub async fn refresh_account_token(
    pool: &AccountPool,
    account_id: Uuid,
    deadline: Deadline,
) -> Result<Account, RefreshError> {
    let account = match pool.repo().get_by_id(account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return Err(RefreshError::Terminal(format!("账号 {account_id} 不存在")));
        }
        Err(error) => return Err(RefreshError::Retryable(error.to_string())),
    };
    let Some(refresh_token) = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Err(RefreshError::Terminal("没有 refresh_token".to_string()));
    };

    let client = pool.client().map_err(|error| RefreshError::Retryable(error.to_string()))?;
    let outcome = refresh_tokens(client, &refresh_token, deadline).await?;

    let updated = pool
        .repo()
        .replace_tokens(
            account_id,
            &outcome.access_token,
            outcome.refresh_token.as_deref(),
            outcome.id_token.as_deref(),
        )
        .await
        .map_err(|error| RefreshError::Retryable(error.to_string()))?;
    let updated = pool
        .clear_invalid(account_id)
        .await
        .unwrap_or(updated);
    Ok(updated)
}

/// 批量同步并等待完成（供 watcher 复用）。
pub async fn run_sync_batch(
    pool: &Arc<AccountPool>,
    progress: &Arc<ProgressStore>,
    account_ids: Vec<Uuid>,
    deadline: Deadline,
    concurrency: usize,
) -> Uuid {
    let operation_id = progress.start(OperationKind::Sync, account_ids.len());
    run_batch(pool, progress, operation_id, account_ids, concurrency, move |pool, id| async move {
        sync_account(&pool, id, deadline)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await;
    progress.finish(operation_id);
    operation_id
}

/// 批量刷新 token 并等待完成（供 watcher 复用）。
pub async fn run_refresh_batch(
    pool: &Arc<AccountPool>,
    progress: &Arc<ProgressStore>,
    account_ids: Vec<Uuid>,
    deadline: Deadline,
    concurrency: usize,
) -> Uuid {
    let operation_id = progress.start(OperationKind::RefreshToken, account_ids.len());
    run_batch(pool, progress, operation_id, account_ids, concurrency, move |pool, id| async move {
        refresh_account_token(&pool, id, deadline)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await;
    progress.finish(operation_id);
    operation_id
}

/// 批量同步（后台任务），立即返回操作 ID。
pub fn spawn_sync_batch(
    pool: Arc<AccountPool>,
    progress: Arc<ProgressStore>,
    account_ids: Vec<Uuid>,
    deadline: Deadline,
    concurrency: usize,
) -> Uuid {
    let operation_id = progress.start(OperationKind::Sync, account_ids.len());
    let store = progress.clone();
    tokio::spawn(async move {
        run_batch(&pool, &store, operation_id, account_ids, concurrency, move |pool, id| async move {
            sync_account(&pool, id, deadline)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await;
        store.finish(operation_id);
    });
    operation_id
}

/// 批量刷新 access token（后台任务），立即返回操作 ID。
pub fn spawn_refresh_batch(
    pool: Arc<AccountPool>,
    progress: Arc<ProgressStore>,
    account_ids: Vec<Uuid>,
    deadline: Deadline,
    concurrency: usize,
) -> Uuid {
    let operation_id = progress.start(OperationKind::RefreshToken, account_ids.len());
    let store = progress.clone();
    tokio::spawn(async move {
        run_batch(&pool, &store, operation_id, account_ids, concurrency, move |pool, id| async move {
            refresh_account_token(&pool, id, deadline)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await;
        store.finish(operation_id);
    });
    operation_id
}

/// 通用批量执行器：受信号量限流，逐条记成功/失败。
async fn run_batch<F, Fut>(
    pool: &Arc<AccountPool>,
    progress: &Arc<ProgressStore>,
    operation_id: Uuid,
    account_ids: Vec<Uuid>,
    concurrency: usize,
    task: F,
) where
    F: Fn(Arc<AccountPool>, Uuid) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let limit = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let task = Arc::new(task);
    let mut set = tokio::task::JoinSet::new();

    for account_id in account_ids {
        let permit = match limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let pool = pool.clone();
        let store = progress.clone();
        let task = task.clone();
        set.spawn(async move {
            let _permit = permit;
            let label = pool
                .repo()
                .get_by_id(account_id)
                .await
                .ok()
                .flatten()
                .map(|account| account.label())
                .unwrap_or_else(|| account_id.to_string());
            match task(pool, account_id).await {
                Ok(()) => store.record_success(operation_id),
                Err(message) => {
                    store.record_failure(operation_id, account_id, label, message)
                }
            }
        });
    }

    while set.join_next().await.is_some() {}
}

/// 判断某账号是否需要 sync（供 watcher 使用）。
pub fn needs_sync(account: &Account, now: chrono::DateTime<Utc>) -> bool {
    account.status == AccountStatus::Limited
        || AccountPool::needs_verification(account, now)
        || account.last_checked_at.is_none()
}

/// 判断某账号是否需要刷新 access token（供 watcher 使用）。
pub fn needs_token_refresh(account: &Account, now: chrono::DateTime<Utc>) -> bool {
    if account.refresh_token.as_deref().unwrap_or("").trim().is_empty() {
        return false;
    }
    crate::pool::needs_access_refresh(account, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Plan;

    fn base_account(status: AccountStatus) -> Account {
        let now = Utc::now();
        Account {
            id: Uuid::new_v4(),
            access_token: "tok".to_string(),
            refresh_token: Some("rt".to_string()),
            id_token: None,
            email: Some("a@b.c".to_string()),
            upstream_account_id: None,
            plan: Plan::Plus,
            source: crate::model::Source::Web,
            status,
            quota: 3,
            quota_confirmed: true,
            restore_at: None,
            proxy: String::new(),
            proxy_group_id: None,
            success_count: 0,
            fail_count: 0,
            invalid_count: 0,
            last_used_at: None,
            last_invalid_at: None,
            last_error: None,
            last_error_at: None,
            last_checked_at: Some(now),
            last_refresh_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn needs_sync_for_limited_account() {
        let account = base_account(AccountStatus::Limited);
        assert!(needs_sync(&account, Utc::now()));
    }

    #[test]
    fn needs_sync_when_never_checked() {
        let mut account = base_account(AccountStatus::Normal);
        account.last_checked_at = None;
        assert!(needs_sync(&account, Utc::now()));
    }

    #[test]
    fn no_sync_when_fresh_and_normal() {
        let account = base_account(AccountStatus::Normal);
        assert!(!needs_sync(&account, Utc::now()));
    }

    #[test]
    fn needs_token_refresh_only_with_refresh_token() {
        let mut account = base_account(AccountStatus::Normal);
        account.access_token = "garbage".to_string(); // 解不出 exp → 需要刷新
        assert!(needs_token_refresh(&account, Utc::now()));

        account.refresh_token = None;
        assert!(!needs_token_refresh(&account, Utc::now()), "没有 RT 无法刷新");
    }
}

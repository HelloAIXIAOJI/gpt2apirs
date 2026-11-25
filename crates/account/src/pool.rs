//! 账号池：内存快照 + 选号 + 并发槽位 + 状态回写。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 按代理串解析上游客户端；由调用方决定「账号代理 / 全局出口 / 直连」。
pub type ClientResolver =
    Arc<dyn Fn(&str) -> anyhow::Result<Arc<gpt2apirs_http::Client>> + Send + Sync>;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;
use gpt2apirs_core::Deadline;
use tokio::sync::{Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use uuid::Uuid;

use crate::consts::{
    ACCOUNT_FRESHNESS_SECS, DEFER_INVALID_MAX_COUNT, INVALID_CONFIRM_SECS,
    NEW_ACCOUNT_GRACE_SECS, SLOT_WAIT_MAX_MS,
};
use crate::credentials::{self, Availability};
use crate::model::{Account, AccountStatus, Plan, Source};
use crate::repo::{AccountError, AccountPatch, AccountRepo};

/// 账号池错误。
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// 没有任何符合条件的账号。
    #[error("无可用账号（筛选：{reason}）")]
    NoCandidate {
        /// 人类可读的筛选描述。
        reason: String,
    },

    /// 等待槽位超时。
    #[error("等待账号槽位超时")]
    DeadlineExceeded,

    /// 仓储错误。
    #[error(transparent)]
    Repo(#[from] AccountError),

    /// 池未配置 HTTP 客户端，无法访问上游。
    #[error("账号池未配置 HTTP 客户端，无法执行上游操作")]
    NoHttpClient,

    /// 解析客户端失败（代理串非法等）。
    #[error("解析上游客户端失败：{0}")]
    HttpClient(String),
}

/// 取号用途。决定状态与配额的准入条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 生图：必须 `Normal` 且还有配额（「不限量」也算有）。
    Image,
    /// 文本：允许 `Limited`，不消耗图片配额。
    Text,
}

/// 选号条件。
#[derive(Debug, Clone, Default)]
pub struct SelectionFilter {
    /// 用途。
    pub purpose: Purpose,
    /// 只看某个套餐。
    pub plan: Option<Plan>,
    /// 只看某个来源。
    pub source: Option<Source>,
    /// 本轮已尝试过的账号，避免重复。
    pub exclude: HashSet<Uuid>,
    /// 只允许这一个账号（测试单个账号时用）。
    pub only: Option<Uuid>,
    /// 只在这个桶里选；`None` = 不限桶（全局 key 走这条）。
    ///
    /// 桶来自 API key：key 绑了桶就只用那个桶的号，
    /// 这样「哪个 key 走哪批号」是能预期的，而不是随机撞。
    pub bucket: Option<Uuid>,
}

impl Default for Purpose {
    fn default() -> Self {
        Purpose::Image
    }
}

impl SelectionFilter {
    /// 文本用途的筛选。
    ///
    /// 与生图的差别：允许 `limited` 账号（文本不吃图片配额）。
    pub fn for_text() -> Self {
        Self {
            purpose: Purpose::Text,
            ..Default::default()
        }
    }

    /// 生图用的默认筛选。
    pub fn for_image() -> Self {
        Self {
            purpose: Purpose::Image,
            ..Default::default()
        }
    }

    fn describe(&self) -> String {
        let mut parts = vec![match self.purpose {
            Purpose::Image => "image",
            Purpose::Text => "text",
        }
        .to_string()];
        if let Some(plan) = self.plan {
            parts.push(format!("plan={}", plan.as_str()));
        }
        if let Some(source) = self.source {
            parts.push(format!("source={}", source.as_str()));
        }
        if let Some(bucket) = self.bucket {
            parts.push(format!("bucket={}", &bucket.to_string()[..8]));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("excluded={}", self.exclude.len()));
        }
        parts.join(" ")
    }
}

/// 池的运行参数。
///
/// 校验不在选号时做：选号只过本地凭证门（JWT 未过期），远端校验走
/// `AccountPool::needs_verification` 的新鲜度规则，由 watcher 周期跑、
/// 或管理面「同步配额」手动触发。别再往这里加「选号时远端校验」的开关 ——
/// 那会让每次取号多一次往返，而失败后本来就会标异常并进后台复核。
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// 单账号同时最多几个生图请求（对应 `image_account_concurrency`）。
    pub image_concurrency: u32,
    /// 失效时是否直接删除账号。`false` = 只标异常（**默认，非破坏性**）。
    pub auto_remove_invalid: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            // 默认单账号 1 个并发：上游对同账号并发很敏感
            image_concurrency: 1,
            auto_remove_invalid: false,
        }
    }
}

/// 失效处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidOutcome {
    /// 处于保护期，只累加计数，未处置。
    Deferred,
    /// 已标记为异常。
    MarkedAbnormal,
    /// 已删除。
    Removed,
}

/// 一次取号租约。`Drop` 时自动归还槽位。
pub struct AccountLease {
    account: Arc<Account>,
    notify: Arc<Notify>,
    _permit: OwnedSemaphorePermit,
}

impl AccountLease {
    /// 账号快照。
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// 账号 ID。
    pub fn id(&self) -> Uuid {
        self.account.id
    }

    /// access token。
    pub fn access_token(&self) -> &str {
        &self.account.access_token
    }
}

impl std::fmt::Debug for AccountLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountLease")
            .field("id", &self.account.id)
            .field("label", &self.account.label())
            .finish()
    }
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        // permit 在本函数返回后才释放；先唤醒等待者，它们会在下一轮重新检查，
        // 因此这一记「早唤醒」是无害的。
        self.notify.notify_waiters();
    }
}

/// 账号池。
pub struct AccountPool {
    repo: AccountRepo,
    config: PoolConfig,
    snapshot: RwLock<Arc<Vec<Arc<Account>>>>,
    slots: DashMap<Uuid, Arc<Semaphore>>,
    cursor: AtomicUsize,
    notify: Arc<Notify>,
    /// 按代理串取客户端（账号代理 → 全局出口 → 直连由外层实现）。
    clients: std::sync::RwLock<Option<ClientResolver>>,
    user_agent: String,
}

impl std::fmt::Debug for AccountPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPool")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AccountPool {
    /// 构造（无 HTTP 能力，只能做本地选号）。
    pub fn new(repo: AccountRepo, config: PoolConfig) -> Self {
        Self {
            repo,
            config,
            snapshot: RwLock::new(Arc::new(Vec::new())),
            slots: DashMap::new(),
            cursor: AtomicUsize::new(0),
            notify: Arc::new(Notify::new()),
            clients: std::sync::RwLock::new(None),
            user_agent: String::new(),
        }
    }

    /// 挂上客户端解析器，解锁同步/刷新能力。
    ///
    /// 解析器按「账号代理 → 全局出口 → 直连」取客户端；
    /// 所以出口换节点后这里不需要重建。
    pub fn with_resolver(mut self, resolver: ClientResolver, user_agent: impl Into<String>) -> Self {
        if let Ok(mut guard) = self.clients.write() {
            *guard = Some(resolver);
        }
        self.user_agent = user_agent.into();
        self
    }


    /// 仓储引用。
    pub fn repo(&self) -> &AccountRepo {
        &self.repo
    }

    /// 参数。
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// 当前快照长度。
    pub async fn len(&self) -> usize {
        self.snapshot.read().await.len()
    }

    /// 是否为空。
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// 从数据库全量重载快照。
    ///
    /// 同时按需补建/清理槽位表，保证「账号新增有槽、账号删除无槽」。
    pub async fn reload(&self) -> Result<usize, PoolError> {
        let accounts = self.repo.list_all().await?;
        let ids: HashSet<Uuid> = accounts.iter().map(|a| a.id).collect();

        // 先建好新账号的槽位，避免选号时临时创建造成抖动
        for id in &ids {
            self.ensure_slot(*id);
        }
        // 清理已删除账号的槽位
        let stale: Vec<Uuid> = self
            .slots
            .iter()
            .map(|entry| *entry.key())
            .filter(|id| !ids.contains(id))
            .collect();
        for id in stale {
            self.slots.remove(&id);
        }

        let snapshot: Vec<Arc<Account>> = accounts.into_iter().map(Arc::new).collect();
        let len = snapshot.len();
        *self.snapshot.write().await = Arc::new(snapshot);
        Ok(len)
    }

    /// 当前快照（克隆一份 Arc，调用方持有期间不受后续重载影响）。
    pub async fn snapshot(&self) -> Arc<Vec<Arc<Account>>> {
        self.snapshot.read().await.clone()
    }

    /// 单条更新快照（避免为一次状态变更做全量重载）。
    async fn upsert_snapshot(&self, account: Account) {
        self.ensure_slot(account.id);
        let mut guard = self.snapshot.write().await;
        let mut next: Vec<Arc<Account>> = (**guard).clone();
        match next.iter_mut().find(|item| item.id == account.id) {
            Some(slot) => *slot = Arc::new(account),
            None => next.push(Arc::new(account)),
        }
        *guard = Arc::new(next);
    }

    async fn remove_from_snapshot(&self, id: Uuid) {
        self.slots.remove(&id);
        let mut guard = self.snapshot.write().await;
        let next: Vec<Arc<Account>> = guard.iter().filter(|a| a.id != id).cloned().collect();
        *guard = Arc::new(next);
    }

    fn ensure_slot(&self, id: Uuid) {
        let permits = self.config.image_concurrency.max(1) as usize;
        self.slots
            .entry(id)
            .or_insert_with(|| Arc::new(Semaphore::new(permits)));
    }

    fn slot_for(&self, id: Uuid) -> Arc<Semaphore> {
        self.ensure_slot(id);
        self.slots
            .get(&id)
            .map(|entry| entry.clone())
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)))
    }

    /// 从快照里筛出候选（四层筛选的 ①②③）。
    pub fn candidates(
        &self,
        snapshot: &[Arc<Account>],
        filter: &SelectionFilter,
    ) -> Vec<Arc<Account>> {
        candidates(snapshot, filter)
    }

    /// 取一个账号。返回的租约在 `Drop` 时归还槽位。
    pub async fn acquire(
        &self,
        filter: &SelectionFilter,
        deadline: Deadline,
    ) -> Result<AccountLease, PoolError> {
        loop {
            let snapshot = self.snapshot().await;
            let ready = self.candidates(&snapshot, filter);
            if ready.is_empty() {
                return Err(PoolError::NoCandidate {
                    reason: filter.describe(),
                });
            }

            let start = self.cursor.fetch_add(1, Ordering::Relaxed);
            for offset in 0..ready.len() {
                let account = &ready[(start + offset) % ready.len()];
                let semaphore = self.slot_for(account.id);
                if let Ok(permit) = semaphore.try_acquire_owned() {
                    return Ok(AccountLease {
                        account: account.clone(),
                        notify: self.notify.clone(),
                        _permit: permit,
                    });
                }
            }

            // 全忙：等到有槽释放，或总时限到期
            let wait = match deadline.remaining() {
                Some(left) => left.min(Duration::from_millis(SLOT_WAIT_MAX_MS)),
                None => {
                    if deadline.is_unlimited() {
                        Duration::from_millis(SLOT_WAIT_MAX_MS)
                    } else {
                        return Err(PoolError::DeadlineExceeded);
                    }
                }
            };
            if wait.is_zero() {
                return Err(PoolError::DeadlineExceeded);
            }
            let notified = self.notify.notified();
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(wait) => {}
            }
            if !deadline.is_unlimited() && deadline.is_expired() {
                return Err(PoolError::DeadlineExceeded);
            }
        }
    }

    /// 记录一次使用结果。
    pub async fn mark_result(&self, id: Uuid, success: bool) -> Result<Account, PoolError> {
        let account = self.repo.record_result(id, success).await?;
        self.upsert_snapshot(account.clone()).await;
        Ok(account)
    }

    /// 记录一次**文本**用途的结果（不扣图片配额）。
    pub async fn mark_text_result(&self, id: Uuid, success: bool) -> Result<Account, PoolError> {
        let account = self.repo.record_text_result(id, success).await?;
        self.upsert_snapshot(account.clone()).await;
        Ok(account)
    }

    /// 处置一次「账号被判无效」。
    pub async fn handle_invalid(&self, id: Uuid, error: &str) -> Result<InvalidOutcome, PoolError> {
        let account = self.repo.record_invalid(id, error).await?;
        if should_defer_invalid(&account, Utc::now()) {
            self.upsert_snapshot(account).await;
            return Ok(InvalidOutcome::Deferred);
        }
        if self.config.auto_remove_invalid {
            self.repo.delete_many(&[id]).await?;
            self.remove_from_snapshot(id).await;
            Ok(InvalidOutcome::Removed)
        } else {
            let updated = self
                .repo
                .patch(
                    id,
                    &AccountPatch {
                        status: Some(AccountStatus::Abnormal),
                        quota: Some(0),
                        ..Default::default()
                    },
                )
                .await?;
            self.upsert_snapshot(updated).await;
            Ok(InvalidOutcome::MarkedAbnormal)
        }
    }

    /// 刷新成功后的清理（清失效痕迹、转回正常）。
    pub async fn clear_invalid(&self, id: Uuid) -> Result<Account, PoolError> {
        let account = self.repo.clear_invalid(id).await?;
        self.upsert_snapshot(account.clone()).await;
        Ok(account)
    }

    /// 全量统计。
    pub async fn stats(&self) -> Result<crate::repo::AccountPoolStats, PoolError> {
        Ok(self.repo.stats().await?)
    }

    /// 是否配置了 HTTP 客户端。
    pub fn has_http(&self) -> bool {
        self.clients
            .read()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    pub(crate) fn client(&self, proxy: &str) -> Result<Arc<gpt2apirs_http::Client>, PoolError> {
        let resolver = self
            .clients
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or(PoolError::NoHttpClient)?;
        resolver(proxy).map_err(|error| PoolError::HttpClient(error.to_string()))
    }

    pub(crate) fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// 账号是否需要远端校验（D5 的新鲜度规则）。
    pub fn needs_verification(account: &Account, now: DateTime<Utc>) -> bool {
        if !account.quota_confirmed {
            return true;
        }
        match account.last_checked_at {
            None => true,
            Some(checked) => {
                (now - checked).num_seconds() > ACCOUNT_FRESHNESS_SECS
            }
        }
    }
}

/// 从快照里筛出候选。
///
/// 四层筛选的 ①②③；④（并发有空位）由 [`AccountPool::acquire`] 负责。
pub fn candidates(snapshot: &[Arc<Account>], filter: &SelectionFilter) -> Vec<Arc<Account>> {
    let now = Utc::now();
    snapshot
        .iter()
        .filter(|account| {
            if filter.exclude.contains(&account.id) {
                return false;
            }
            if let Some(only) = filter.only {
                if account.id != only {
                    return false;
                }
            }
            if let Some(plan) = filter.plan {
                if account.plan != plan {
                    return false;
                }
            }
            if let Some(source) = filter.source {
                if account.source != source {
                    return false;
                }
            }
            if let Some(bucket) = filter.bucket {
                if account.bucket_id != bucket {
                    return false;
                }
            }
            // ① 业务准入
            let business_ok = match filter.purpose {
                Purpose::Image => account.status.selectable_for_image() && account.has_image_quota(),
                Purpose::Text => account.status.selectable_for_text(),
            };
            if !business_ok {
                return false;
            }
            // ② 凭证准入
            let projected = credentials::project(
                &account.access_token,
                account.refresh_token.as_deref().unwrap_or(""),
                false,
                false,
                now,
            );
            projected.usable != Availability::Unavailable
        })
        .cloned()
        .collect()
}

/// 是否应当**暂缓**把账号判为失效。
///
/// 三条保护期，命中任一条就只累加计数、不处置。
pub fn should_defer_invalid(account: &Account, now: DateTime<Utc>) -> bool {
    if (now - account.created_at).num_seconds() < NEW_ACCOUNT_GRACE_SECS {
        return true;
    }
    if account.invalid_count <= DEFER_INVALID_MAX_COUNT {
        return true;
    }
    match account.last_invalid_at {
        Some(last) => (now - last).num_seconds() < INVALID_CONFIRM_SECS,
        None => true,
    }
}

/// 计算「距离该刷新 token」的判定，供 watcher 使用。
///
/// 返回 `true` 表示应尽快刷新 access_token。
pub fn needs_access_refresh(account: &Account, now: DateTime<Utc>) -> bool {
    let skewed = now + ChronoDuration::seconds(crate::consts::ACCESS_REFRESH_SKEW_SECS);
    match credentials::token_exp(&account.access_token) {
        None => true,
        Some(exp) => exp <= skewed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountStatus, Plan, Source};
    use base64::Engine as _;

    fn jwt_with_exp(exp_secs: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp_secs}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    fn account(suffix: &str) -> Account {
        let now = Utc::now();
        let exp = (now + ChronoDuration::days(30)).timestamp();
        Account {
            id: Uuid::new_v4(),
            bucket_id: Uuid::nil(),
            access_token: format!("tok-{suffix}-{}", jwt_with_exp(exp)),
            refresh_token: Some("rt".to_string()),
            id_token: None,
            email: Some(format!("{suffix}@test.local")),
            upstream_account_id: None,
            plan: Plan::Plus,
            source: Source::Web,
            status: AccountStatus::Normal,
            quota: 5,
            quota_confirmed: true,
            quota_unlimited: false,
            restore_at: None,
            success_count: 0,
            fail_count: 0,
            invalid_count: 0,
            last_used_at: None,
            last_invalid_at: None,
            last_error: None,
            last_error_at: None,
            last_checked_at: Some(now),
            last_refresh_at: None,
            created_at: now - ChronoDuration::days(1),
            updated_at: now,
        }
    }

    /// 造一个不触库的池，只测纯逻辑。
    fn logic_only_pool(config: PoolConfig) -> AccountPool {
        // 用一个永不连接的 pool 只为满足类型；测试里不调用仓储方法。
        let opts = sqlx::postgres::PgConnectOptions::new()
            .host("127.0.0.1")
            .port(1)
            .database("none");
        let pg = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with(opts);
        AccountPool::new(AccountRepo::new(pg), config)
    }

    #[test]
    fn candidates_skip_limited_for_image() {
        let mut limited = account("limited");
        limited.status = AccountStatus::Limited;
        let normal = account("normal");
        let snap: Vec<Arc<Account>> = vec![Arc::new(limited), Arc::new(normal.clone())];

        let ready = candidates(&snap, &SelectionFilter::for_image());
        assert_eq!(ready.len(), 1, "图片只应选正常账号");
        assert_eq!(ready[0].id, normal.id);
    }

    #[test]
    fn candidates_allow_limited_for_text() {
        let mut limited = account("limited");
        limited.status = AccountStatus::Limited;
        let snap: Vec<Arc<Account>> = vec![Arc::new(limited)];

        let filter = SelectionFilter {
            purpose: Purpose::Text,
            ..Default::default()
        };
        assert_eq!(candidates(&snap, &filter).len(), 1, "文本可用限流号");
    }

    #[test]
    fn candidates_skip_zero_quota_for_image() {
        let mut empty = account("empty");
        empty.quota = 0;
        let snap: Vec<Arc<Account>> = vec![Arc::new(empty)];
        assert!(candidates(&snap, &SelectionFilter::for_image()).is_empty());
    }

    #[test]
    fn candidates_skip_unavailable_credential() {
        let mut dead = account("dead");
        // 已过期且没有 refresh token  Unavailable
        dead.access_token = jwt_with_exp(Utc::now().timestamp() - 3600);
        dead.refresh_token = None;
        let snap: Vec<Arc<Account>> = vec![Arc::new(dead)];
        assert!(
            candidates(&snap, &SelectionFilter::for_image()).is_empty(),
            "凭证彻底不可用的账号不应入选"
        );
    }

    #[test]
    fn candidates_respect_exclude_and_filters() {
        let a = account("a");
        let mut b = account("b");
        b.plan = Plan::Free;
        let snap: Vec<Arc<Account>> = vec![Arc::new(a.clone()), Arc::new(b.clone())];

        let mut exclude = HashSet::new();
        exclude.insert(a.id);
        let filtered = candidates(
            &snap,
            &SelectionFilter {
                purpose: Purpose::Image,
                exclude,
                ..Default::default()
            },
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, b.id);

        let by_plan = candidates(
            &snap,
            &SelectionFilter {
                purpose: Purpose::Image,
                plan: Some(Plan::Free),
                ..Default::default()
            },
        );
        assert_eq!(by_plan.len(), 1);
        assert_eq!(by_plan[0].id, b.id);
    }

    #[test]
    fn candidates_honour_only_restriction() {
        let a = account("a");
        let b = account("b");
        let snap: Vec<Arc<Account>> = vec![Arc::new(a.clone()), Arc::new(b)];
        let picked = candidates(
            &snap,
            &SelectionFilter {
                purpose: Purpose::Image,
                only: Some(a.id),
                ..Default::default()
            },
        );
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, a.id);
    }

    #[test]
    fn defer_invalid_for_brand_new_account() {
        let now = Utc::now();
        let mut fresh = account("fresh");
        fresh.created_at = now - ChronoDuration::seconds(60);
        fresh.invalid_count = 99;
        fresh.last_invalid_at = Some(now - ChronoDuration::seconds(3600));
        assert!(should_defer_invalid(&fresh, now), "新账号应受保护");
    }

    #[test]
    fn defer_invalid_for_first_failure() {
        let now = Utc::now();
        let mut once = account("once");
        once.created_at = now - ChronoDuration::days(10);
        once.invalid_count = 1;
        once.last_invalid_at = Some(now - ChronoDuration::seconds(3600));
        assert!(should_defer_invalid(&once, now), "首次失效不算数");
    }

    #[test]
    fn defer_invalid_within_confirm_window() {
        let now = Utc::now();
        let mut recent = account("recent");
        recent.created_at = now - ChronoDuration::days(10);
        recent.invalid_count = 5;
        recent.last_invalid_at = Some(now - ChronoDuration::seconds(5));
        assert!(should_defer_invalid(&recent, now), "30 秒内重复失败应暂缓");
    }

    #[test]
    fn confirms_invalid_after_all_protections() {
        let now = Utc::now();
        let mut dead = account("dead");
        dead.created_at = now - ChronoDuration::days(10);
        dead.invalid_count = 5;
        dead.last_invalid_at = Some(now - ChronoDuration::seconds(3600));
        assert!(!should_defer_invalid(&dead, now), "三重保护期都过了应处置");
    }

    #[test]
    fn needs_verification_when_unconfirmed_or_stale() {
        let now = Utc::now();
        let mut fresh = account("fresh");
        fresh.last_checked_at = Some(now);
        assert!(!AccountPool::needs_verification(&fresh, now), "刚校验过不需要");

        let mut unconfirmed = fresh.clone();
        unconfirmed.quota_confirmed = false;
        assert!(AccountPool::needs_verification(&unconfirmed, now), "未确认必须校验");

        let mut stale = fresh.clone();
        stale.last_checked_at = Some(now - ChronoDuration::seconds(ACCOUNT_FRESHNESS_SECS + 10));
        assert!(AccountPool::needs_verification(&stale, now), "过期必须校验");

        let mut never = fresh.clone();
        never.last_checked_at = None;
        assert!(AccountPool::needs_verification(&never, now), "从未校验必须校验");
    }

    #[test]
    fn needs_access_refresh_near_expiry() {
        let now = Utc::now();
        let mut soon = account("soon");
        soon.access_token = jwt_with_exp((now + ChronoDuration::hours(2)).timestamp());
        assert!(needs_access_refresh(&soon, now), "2 小时内到期应刷新");

        let mut far = account("far");
        far.access_token = jwt_with_exp((now + ChronoDuration::days(30)).timestamp());
        assert!(!needs_access_refresh(&far, now), "30 天后到期不用刷");

        let mut broken = account("broken");
        broken.access_token = "garbage".to_string();
        assert!(needs_access_refresh(&broken, now), "解不出 exp 也应尝试刷新");
    }

    #[tokio::test]
    async fn acquire_returns_no_candidate_when_snapshot_empty() {
        let pool = logic_only_pool(PoolConfig::default());
        let error = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(1.0))
            .await
            .expect_err("空池应报 NoCandidate");
        assert!(matches!(error, PoolError::NoCandidate { .. }), "{error}");
    }

    #[tokio::test]
    async fn acquire_respects_concurrency_and_deadline() {
        let pool = logic_only_pool(PoolConfig {
            image_concurrency: 1,
            ..Default::default()
        });
        let one = account("one");
        *pool.snapshot.write().await = Arc::new(vec![Arc::new(one.clone())]);

        let lease = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(2.0))
            .await
            .expect("首次应成功");
        assert_eq!(lease.id(), one.id);

        // 槽位已被占用  应超时
        let error = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(1.0))
            .await
            .expect_err("槽位占满应超时");
        assert!(matches!(error, PoolError::DeadlineExceeded), "{error}");

        // 归还后应能再次取到
        drop(lease);
        let again = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(2.0))
            .await
            .expect("归还后应可再取");
        assert_eq!(again.id(), one.id);
    }

    #[tokio::test]
    async fn two_accounts_round_robin() {
        let pool = logic_only_pool(PoolConfig {
            image_concurrency: 1,
            ..Default::default()
        });
        let a = account("a");
        let b = account("b");
        *pool.snapshot.write().await = Arc::new(vec![Arc::new(a.clone()), Arc::new(b.clone())]);

        let first = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(2.0))
            .await
            .expect("第一次");
        let second = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(2.0))
            .await
            .expect("第二次");
        assert_ne!(first.id(), second.id(), "应轮询到另一个账号");
        drop(first);
        drop(second);

        // 两个都归还后，再来两次仍应覆盖两个账号
        let mut seen = HashSet::new();
        for _ in 0..2 {
            let lease = pool
                .acquire(&SelectionFilter::for_image(), Deadline::in_secs(2.0))
                .await
                .expect("再来一次");
            seen.insert(lease.id());
            drop(lease);
        }
        assert_eq!(seen.len(), 2, "轮询应覆盖两个账号");
    }

    #[tokio::test]
    async fn lease_drop_is_idempotent_and_frees_slot() {
        let pool = logic_only_pool(PoolConfig::default());
        *pool.snapshot.write().await = Arc::new(vec![Arc::new(account("x"))]);
        {
            let _lease = pool
                .acquire(&SelectionFilter::for_image(), Deadline::in_secs(1.0))
                .await
                .expect("取号");
        }
        // 作用域结束后槽位应已归还
        let _again = pool
            .acquire(&SelectionFilter::for_image(), Deadline::in_secs(1.0))
            .await
            .expect("应可再取");
    }

    #[tokio::test]
    async fn resolver_receives_account_proxy() {
        // 不变量：账号自己的 proxy 必须原样交给解析器，不能被忽略成直连。
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = seen.clone();
        let resolver: ClientResolver = std::sync::Arc::new(move |proxy: &str| {
            recorder.lock().expect("锁").push(proxy.to_string());
            Ok(std::sync::Arc::new(
                gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default())
                    .expect("客户端"),
            ))
        });
        let pool = logic_only_pool(PoolConfig::default()).with_resolver(resolver, "ua");

        pool.client("http://127.0.0.1:7890").expect("应能取到客户端");
        pool.client("").expect("空串表示走全局出口");
        assert_eq!(
            seen.lock().expect("锁").clone(),
            vec!["http://127.0.0.1:7890".to_string(), String::new()]
        );
    }

    #[tokio::test]
    async fn client_without_resolver_reports_no_http() {
        let pool = logic_only_pool(PoolConfig::default());
        assert!(matches!(pool.client(""), Err(PoolError::NoHttpClient)));
    }

}


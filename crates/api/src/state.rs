//! 应用共享状态。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpt2apirs_account::{AccountPool, OAuthSessions, ProgressStore};
use gpt2apirs_core::{BootstrapConfig, RuntimeSettings};
use gpt2apirs_http::Client;
use gpt2apirs_storage::repo::ApiKeyStore;
use sqlx::PgPool;

/// 所有 handler 共享的依赖。
#[derive(Clone)]
pub struct AppState {
    /// 引导层配置（不可热改）。
    pub config: Arc<BootstrapConfig>,
    /// 运行层设置（可热改）。
    pub settings: Arc<RuntimeSettings>,
    /// 数据库连接池。
    pub db: PgPool,
    /// 上游 HTTP 客户端（TLS 指纹）。
    pub client: Arc<Client>,
    /// 账号池。
    pub pool: Arc<AccountPool>,
    /// 批量操作进度。
    pub progress: Arc<ProgressStore>,
    /// OAuth 授权会话。
    pub oauth: Arc<OAuthSessions>,
    /// 调用 `/v1/*` 的 API key 存储。
    pub api_keys: Arc<ApiKeyStore>,
    /// 后台会话签名密钥（由 `ADMIN_KEY` 派生）。
    pub session_secret: Arc<[u8; 32]>,
    /// 进程启动时间（用于 uptime）。
    pub started_at: DateTime<Utc>,
}

impl AppState {
    /// 构造。
    pub fn new(
        config: Arc<BootstrapConfig>,
        settings: Arc<RuntimeSettings>,
        db: PgPool,
        client: Arc<Client>,
        pool: Arc<AccountPool>,
        progress: Arc<ProgressStore>,
        oauth: Arc<OAuthSessions>,
        api_keys: Arc<ApiKeyStore>,
    ) -> Self {
        let session_secret = Arc::new(crate::session::derive_secret(&config.admin_key));
        Self {
            config,
            settings,
            db,
            client,
            pool,
            progress,
            oauth,
            api_keys,
            session_secret,
            started_at: Utc::now(),
        }
    }

    /// 已运行秒数。
    pub fn uptime_secs(&self) -> i64 {
        (Utc::now() - self.started_at).num_seconds().max(0)
    }

    /// 对外基础地址：优先配置，否则按请求推导。
    pub fn public_base_url(&self, scheme: &str, host: &str) -> String {
        match self.config.public_base_url.as_deref() {
            Some(configured) if !configured.is_empty() => configured.to_string(),
            _ => format!("{scheme}://{host}"),
        }
    }
}

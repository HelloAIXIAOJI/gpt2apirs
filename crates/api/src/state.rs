//! 应用共享状态。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
    /// 按代理串缓存的客户端（空串代表直连）。
    client_cache: Arc<RwLock<HashMap<String, Arc<Client>>>>,
    /// 进程启动时间（用于 uptime）。
    pub started_at: DateTime<Utc>,
    /// 异步生图任务存储。
    pub image_tasks: Arc<crate::image_tasks::ImageTaskStore>,
    /// 管理面登录失败节流。
    pub login_throttle: Arc<crate::login_throttle::LoginThrottle>,
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
        let task_concurrency = settings.get_i64("image_task_concurrency").clamp(1, 16) as usize;
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
            client_cache: Arc::new(RwLock::new(HashMap::new())),
            started_at: Utc::now(),
            image_tasks: Arc::new(crate::image_tasks::ImageTaskStore::new(task_concurrency)),
            login_throttle: Arc::new(crate::login_throttle::LoginThrottle::new()),
        }
    }

    /// 取该账号要用的客户端。
    ///
    /// 账号配了 `proxy` 就按代理串缓存一个客户端，没配就用共享的那个。
    /// 缓存是必要的：每次新建客户端都要重建 TLS 配置。
    pub fn client_for(&self, proxy: &str) -> anyhow::Result<Arc<Client>> {
        let key = proxy.trim().to_string();
        if key.is_empty() {
            return Ok(self.client.clone());
        }
        if let Ok(guard) = self.client_cache.read() {
            if let Some(client) = guard.get(&key) {
                return Ok(client.clone());
            }
        }
        let client = Arc::new(Client::new(gpt2apirs_http::ClientOptions {
            emulation: Some(gpt2apirs_http::Emulation::Chrome110),
            proxy: Some(key.clone()),
            timeout_secs: Some(180),
        })?);
        if let Ok(mut guard) = self.client_cache.write() {
            guard.insert(key, client.clone());
        }
        Ok(client)
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

//! 账号池的业务常量。
//!
//! 集中定义并注明来源，避免散落在各处成为魔法值。

// 选号与校验

/// 候选账号远端校验结果的新鲜期（秒）。上游配额按分钟量级变化。
pub const ACCOUNT_FRESHNESS_SECS: i64 = 300;

/// 槽位等待的轮询上限（毫秒）。作 `Notify` 的保护性兜底，防止丢失唤醒后无限挂起。
pub const SLOT_WAIT_MAX_MS: u64 = 1_000;

// 失效判定保护期

/// 新账号保护期（秒）：刚导入的账号可能还没生效。
pub const NEW_ACCOUNT_GRACE_SECS: i64 = 600;

/// 同类失效的确认窗口（秒）：短时间重复失败可能是一过性问题。
pub const INVALID_CONFIRM_SECS: i64 = 30;

/// `invalid_count` 不超过该值时永不判死刑（首次失效不算数）。
pub const DEFER_INVALID_MAX_COUNT: i32 = 1;

// OAuth

/// OpenAI OAuth token 端点。
pub const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// OpenAI OAuth 授权端点（PKCE 起始）。
pub const OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";

/// 平台 OAuth 客户端 ID（公开值，非密钥）。
pub const OAUTH_CLIENT_ID: &str = "app_2SKx67EdpoN0G6j64rFvigXD";

/// OAuth 受众。
pub const OAUTH_AUDIENCE: &str = "https://api.openai.com/v1";

/// 回调地址。
pub const OAUTH_REDIRECT_URI: &str = "https://platform.openai.com/auth/callback";

/// 申请的作用域。`offline_access` 是拿到 refresh_token 的前提。
pub const OAUTH_SCOPE: &str = "openid profile email offline_access";

/// OAuth 登录会话的有效期（秒）。
pub const OAUTH_SESSION_TTL_SECS: i64 = 600;

// 后台刷新节流

/// token 刷新失败后的退避（秒）。
pub const REFRESH_BACKOFF_SECS: i64 = 300;

/// refresh_token keepalive 失败后的退避（秒）。
pub const KEEPALIVE_BACKOFF_SECS: i64 = 21_600;

/// access_token 提前刷新的阈值（秒）。
pub const ACCESS_REFRESH_SKEW_SECS: i64 = 86_400;

/// refresh_token 的 keepalive 间隔（秒）。
pub const KEEPALIVE_INTERVAL_SECS: i64 = 259_200;

/// 每轮 keepalive 处理的账号数。
pub const KEEPALIVE_BATCH: usize = 3;

/// keepalive 单轮最多处理多少个账号（防止一轮打太久）。
pub const KEEPALIVE_MAX_PER_ROUND: usize = 30;

// 导入

/// 导入时单批最多处理多少条，避免一次事务过大。
pub const IMPORT_BATCH_SIZE: usize = 100;

/// 操作进度在内存中保留的时长（秒），供前端轮询。
pub const PROGRESS_RETENTION_SECS: i64 = 300;

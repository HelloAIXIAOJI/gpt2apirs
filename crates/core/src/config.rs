//! 引导层配置（来自 `.env` / 环境变量）。
//!
//! 设计见 `docs/m1-design.md` 第三章。
//!
//! # 为什么单独一层
//!
//! 两层配置：**引导层**（本模块）放 `.env` —— 连库必需的、日志级别、监听地址；
//! **运行层**放 PG，是可热改的业务参数（见 [`crate::settings`]）。引导层读不了库，所以不能混。
//!
//! 判据：连数据库本身需要的东西必须留在 `.env`。

use std::time::Duration;

use crate::env::{self, EnvError, Lookup};

/// 默认监听端口。
pub const DEFAULT_PORT: u16 = 8000;
/// 默认监听地址。
pub const DEFAULT_HOST: &str = "0.0.0.0";
/// 默认连接池上限。
pub const DEFAULT_POOL_MAX_CONNECTIONS: u32 = 10;
/// 连接池上限的钳制范围。
pub const POOL_MAX_CONNECTIONS_RANGE: (u32, u32) = (1, 200);
/// `.env` 模板里的占位 key，命中时给警告。
pub const PLACEHOLDER_AUTH_KEY: &str = "gpt2apirs-dev-key-change-me";
/// AUTH_KEY 最小长度。
pub const MIN_AUTH_KEY_LEN: usize = 8;

/// 引导层配置。
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// PostgreSQL 连接串。
    pub database_url: String,
    /// 登录后台与管理面的口令（`ADMIN_KEY`）。
    ///
    /// 与调用 `/v1/*` 的 API key 完全分离：把登录口令发给客户端是错的。
    pub admin_key: String,
    /// 旧的单口令。仅用于把它一次性迁成 API key（见 `ApiKeyRepo::import_legacy`）。
    pub auth_key: String,
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 连接池上限。
    pub pool_max_connections: u32,
    /// 获取连接的超时。
    pub pool_acquire_timeout: Duration,
    /// 连接空闲回收时间。
    pub pool_idle_timeout: Option<Duration>,
    /// 连接最大存活时间。
    pub pool_max_lifetime: Option<Duration>,
    /// `RUST_LOG` 原始值。
    pub rust_log: String,
    /// 运行层日志级别白名单（来自 `LOG_LEVELS`）。
    pub log_levels: Vec<String>,

    /// 对外可访问的基础地址，用于拼接图片 URL。留空则用请求的 Host。
    pub public_base_url: Option<String>,
    /// 加载过程中产生的告警（此时日志可能还没初始化，由调用方负责输出）。
    pub warnings: Vec<String>,
}

impl BootstrapConfig {
    /// 从进程环境（应先调用 `env::load_dotenv()`）读取。
    pub fn from_env() -> Result<Self, EnvError> {
        Self::from_lookup(&env::read_var)
    }

    /// 从任意查找函数读取（可测试）。
    pub fn from_lookup(lookup: &Lookup<'_>) -> Result<Self, EnvError> {
        let mut warnings = Vec::new();

        // ---- 必填：DATABASE_URL ----
        let database_url = env::required_from(
            lookup,
            "DATABASE_URL",
            "请在与可执行文件同目录的 .env 中填写，形如 \
             postgres://user:pass@host:5432/dbname?sslmode=disable",
        )?;
        env::validate_postgres_url(&database_url)?;

        // ---- 登录口令：ADMIN_KEY，缺失时回退旧的 AUTH_KEY ----
        let admin_key = match lookup("ADMIN_KEY").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            Some(value) => value,
            None => {
                warnings.push(
                    "未设置 ADMIN_KEY，暂时沿用 AUTH_KEY 作为后台登录口令；请尽快拆开".to_string(),
                );
                String::new()
            }
        };
        let admin_key = if admin_key.is_empty() {
            // 由下面 AUTH_KEY 的读取结果补齐（保持「缺 AUTH_KEY 就报错」的旧行为）
            String::new()
        } else {
            admin_key
        };

        // ---- 必填：AUTH_KEY ----
        let auth_key = env::required_from(
            lookup,
            "AUTH_KEY",
            "请在与可执行文件同目录的 .env 中填写 AUTH_KEY（至少 8 个字符）",
        )?;
        if auth_key.chars().count() < MIN_AUTH_KEY_LEN {
            return Err(EnvError::Invalid {
                name: "AUTH_KEY",
                value: auth_key.clone(),
                reason: format!("长度必须至少 {MIN_AUTH_KEY_LEN} 个字符"),
            });
        }
        if auth_key == PLACEHOLDER_AUTH_KEY {
            warnings.push(format!(
                "AUTH_KEY 仍是 .env 模板里的占位值 ({PLACEHOLDER_AUTH_KEY})，生产环境务必更换"
            ));
        }

        // ---- 监听 ----
        let host = lookup("HOST").unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port: u16 = env::parsed_from(lookup, "PORT", DEFAULT_PORT)?;
        if port == 0 {
            return Err(EnvError::Invalid {
                name: "PORT",
                value: "0".to_string(),
                reason: "必须在 1..=65535".to_string(),
            });
        }

        // ---- 连接池 ----
        let raw_max: u32 =
            env::parsed_from(lookup, "DATABASE_MAX_CONNECTIONS", DEFAULT_POOL_MAX_CONNECTIONS)?;
        let (lo, hi) = POOL_MAX_CONNECTIONS_RANGE;
        let pool_max_connections = if raw_max < lo || raw_max > hi {
            let clamped = raw_max.clamp(lo, hi);
            warnings.push(format!(
                "DATABASE_MAX_CONNECTIONS={raw_max} 超出 {lo}..={hi}，已钳制为 {clamped}"
            ));
            clamped
        } else {
            raw_max
        };

        let pool_acquire_timeout = Duration::from_secs(
            env::parsed_from(lookup, "DATABASE_ACQUIRE_TIMEOUT_SECONDS", 30u64)?.max(1),
        );
        let pool_idle_timeout = optional_secs(lookup, "DATABASE_IDLE_TIMEOUT_SECONDS", 600);
        let pool_max_lifetime = optional_secs(lookup, "DATABASE_MAX_LIFETIME_SECONDS", 1800);

        // ---- 日志 ----
        let rust_log = lookup("RUST_LOG").unwrap_or_else(|| "info".to_string());
        let log_levels = lookup("LOG_LEVELS")
            .map(|raw| {
                raw.split(',')
                    .map(|part| part.trim().to_lowercase())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // M3 起 token 只存在账号池里。残留的 env 变量不再生效，但要明确告知，
        // 否则升级后「改了 .env 不生效」会非常难排查。
        if lookup("GPT2APIRS_ACCESS_TOKEN").is_some() {
            warnings.push(
                "GPT2APIRS_ACCESS_TOKEN 已停用，请删除该变量并通过 POST /api/accounts 导入账号"
                    .to_string(),
            );
        }
        let public_base_url = lookup("PUBLIC_BASE_URL").map(|value| value.trim_end_matches('/').to_string());

        Ok(Self {
            database_url,
            admin_key: if admin_key.is_empty() {
                auth_key.clone()
            } else {
                admin_key
            },
            auth_key,
            host,
            port,
            pool_max_connections,
            pool_acquire_timeout,
            pool_idle_timeout,
            pool_max_lifetime,
            rust_log,
            log_levels,

            public_base_url,
            warnings,
        })
    }

    /// 脱敏后的连接串，可安全打日志。
    pub fn redacted_database_url(&self) -> String {
        env::redact_url(&self.database_url)
    }

    /// 监听地址，例如 `0.0.0.0:8000`。
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn optional_secs(lookup: &Lookup<'_>, key: &'static str, default: u64) -> Option<Duration> {
    match lookup(key) {
        Some(raw) if raw == "0" => None,
        Some(raw) => raw.parse::<u64>().ok().map(Duration::from_secs),
        None => Some(Duration::from_secs(default)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const GOOD_URL: &str = "postgres://u:p@localhost:5432/db?sslmode=disable";

    fn lookup(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            map.get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        }
    }

    fn base() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("DATABASE_URL", GOOD_URL),
            ("AUTH_KEY", "a-good-key-123"),
            ("ADMIN_KEY", "an-admin-key-456"),
        ])
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let map = base();
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).expect("应成功");
        assert_eq!(cfg.database_url, GOOD_URL);
        assert_eq!(cfg.auth_key, "a-good-key-123");
        assert_eq!(cfg.admin_key, "an-admin-key-456", "登录口令应独立于 API key");
        assert_eq!(cfg.host, DEFAULT_HOST);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.pool_max_connections, DEFAULT_POOL_MAX_CONNECTIONS);
        assert_eq!(cfg.pool_acquire_timeout, Duration::from_secs(30));
        assert_eq!(cfg.pool_idle_timeout, Some(Duration::from_secs(600)));
        assert_eq!(cfg.pool_max_lifetime, Some(Duration::from_secs(1800)));
        assert_eq!(cfg.rust_log, "info");
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn missing_database_url_fails() {
        let map = HashMap::from([("AUTH_KEY", "a-good-key-123")]);
        let l = lookup(map);
        let err = BootstrapConfig::from_lookup(&l).unwrap_err();
        assert!(err.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn non_postgres_url_rejected() {
        let mut map = base();
        map.insert("DATABASE_URL", "sqlite:///data/x.db");
        let l = lookup(map);
        let err = BootstrapConfig::from_lookup(&l).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("postgres://"), "错误信息应提示 scheme: {text}");
    }

    #[test]
    fn admin_key_falls_back_to_auth_key_with_warning() {
        let map = HashMap::from([("DATABASE_URL", GOOD_URL), ("AUTH_KEY", "a-good-key-123")]);
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).expect("配置");
        assert_eq!(cfg.admin_key, "a-good-key-123", "缺 ADMIN_KEY 时暂时沿用旧口令");
        assert!(
            cfg.warnings.iter().any(|w| w.contains("ADMIN_KEY")),
            "回退必须告警，否则没人会去拆开"
        );
    }

    #[test]
    fn missing_auth_key_fails() {
        let map = HashMap::from([("DATABASE_URL", GOOD_URL)]);
        let l = lookup(map);
        let err = BootstrapConfig::from_lookup(&l).unwrap_err();
        assert!(err.to_string().contains("AUTH_KEY"));
    }

    #[test]
    fn short_auth_key_rejected() {
        let mut map = base();
        map.insert("AUTH_KEY", "short");
        let l = lookup(map);
        let err = BootstrapConfig::from_lookup(&l).unwrap_err();
        assert!(err.to_string().contains("至少 8"));
    }

    #[test]
    fn placeholder_auth_key_warns() {
        let mut map = base();
        map.insert("AUTH_KEY", PLACEHOLDER_AUTH_KEY);
        map.insert("ADMIN_KEY", "an-admin-key-456");
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).expect("应成功");
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("占位"));
    }

    #[test]
    fn pool_max_connections_is_clamped() {
        let mut map = base();
        map.insert("DATABASE_MAX_CONNECTIONS", "9999");
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).unwrap();
        assert_eq!(cfg.pool_max_connections, 200);
        assert!(cfg.warnings.iter().any(|w| w.contains("钳制")));
    }

    #[test]
    fn port_zero_rejected() {
        let mut map = base();
        map.insert("PORT", "0");
        let l = lookup(map);
        let err = BootstrapConfig::from_lookup(&l).unwrap_err();
        assert!(err.to_string().contains("PORT"));
    }

    #[test]
    fn idle_timeout_zero_means_none() {
        let mut map = base();
        map.insert("DATABASE_IDLE_TIMEOUT_SECONDS", "0");
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).unwrap();
        assert_eq!(cfg.pool_idle_timeout, None);
    }

    #[test]
    fn log_levels_are_split_and_lowered() {
        let mut map = base();
        map.insert("LOG_LEVELS", "INFO, warning , Error");
        let l = lookup(map);
        let cfg = BootstrapConfig::from_lookup(&l).unwrap();
        assert_eq!(cfg.log_levels, vec!["info", "warning", "error"]);
    }

    #[test]
    fn redacted_url_hides_password() {
        let l = lookup(base());
        let cfg = BootstrapConfig::from_lookup(&l).unwrap();
        assert_eq!(
            cfg.redacted_database_url(),
            "postgres://u:***@localhost:5432/db?sslmode=disable"
        );
    }

    #[test]
    fn bind_address_format() {
        let l = lookup(base());
        let cfg = BootstrapConfig::from_lookup(&l).unwrap();
        assert_eq!(cfg.bind_address(), "0.0.0.0:8000");
    }
}

//! PostgreSQL 连接池。

use std::time::Duration;

use gpt2apirs_core::BootstrapConfig;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// 连接失败。
    #[error("PostgreSQL 连接失败（{url}）：{source}")]
    Connect {
        /// 已脱敏的连接串。
        url: String,
        /// 底层错误。
        #[source]
        source: sqlx::Error,
    },

    /// 查询失败。
    #[error("数据库查询失败：{0}")]
    Query(#[from] sqlx::Error),

    /// 迁移失败。
    #[error("数据库迁移失败：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// 桶里还有账号，不能删。
    #[error("桶里还有 {accounts} 个账号，先移走或删掉它们再删桶")]
    BucketBusy {
        /// 剩余账号数。
        accounts: i64,
    },
}

/// 用引导层配置构建连接池。
///
/// 注意：`connect` 会**立即**建立至少一条连接，所以连接串错了会在此处报错，
/// 而不是等到第一次查询。
pub async fn build_pool(cfg: &BootstrapConfig) -> Result<PgPool, StorageError> {
    let mut options = PgPoolOptions::new()
        .max_connections(cfg.pool_max_connections)
        .acquire_timeout(cfg.pool_acquire_timeout)
        // 对应 SQLAlchemy 的 pool_pre_ping
        .test_before_acquire(true)
        // 对应 pool_recycle（更保守）
        .max_lifetime(cfg.pool_max_lifetime.unwrap_or(Duration::from_secs(1800)));

    if let Some(idle) = cfg.pool_idle_timeout {
        options = options.idle_timeout(idle);
    }

    options.connect(&cfg.database_url).await.map_err(|source| {
        StorageError::Connect {
            url: cfg.redacted_database_url(),
            source,
        }
    })
}

/// 健康检查：`select 1`。
pub async fn health_check(pool: &PgPool) -> Result<(), StorageError> {
    sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(pool)
        .await?;
    Ok(())
}

/// 服务端版本字符串。
pub async fn server_version(pool: &PgPool) -> Result<String, StorageError> {
    let version: String = sqlx::query_scalar("select version()")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

/// 当前数据库名。
pub async fn current_database(pool: &PgPool) -> Result<String, StorageError> {
    let name: String = sqlx::query_scalar("select current_database()")
        .fetch_one(pool)
        .await?;
    Ok(name)
}

/// 连接池当前连接数快照（用于监控与排错）。
pub fn pool_stats(pool: &PgPool) -> PoolStats {
    PoolStats {
        size: pool.size(),
        idle: pool.num_idle(),
    }
}

/// 连接池快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// 已建立的连接总数。
    pub size: u32,
    /// 空闲连接数。
    pub idle: usize,
}

//! 数据库迁移。

use sqlx::PgPool;

use crate::pool::StorageError;

/// 执行所有未应用的迁移。
///
/// 幂等，可在每次启动时调用。
pub async fn run_migrations(pool: &PgPool) -> Result<(), StorageError> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(StorageError::Migrate)?;
    Ok(())
}

/// 已应用的迁移列表 `(version, description)`。
pub async fn applied_migrations(pool: &PgPool) -> Result<Vec<(i64, String)>, StorageError> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "select version, description from _sqlx_migrations order by version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

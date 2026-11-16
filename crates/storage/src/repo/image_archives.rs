//! 归档登记仓储。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 一条归档记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImageArchiveRow {
    /// 主键。
    pub id: Uuid,
    /// 包文件名。
    pub file_name: String,
    /// 包体积。
    pub bytes: i64,
    /// 图片张数。
    pub image_count: i32,
    /// 图片原始字节合计。
    pub image_bytes: i64,
    /// 打包时间。
    pub created_at: DateTime<Utc>,
}

/// 归档登记仓储。
#[derive(Debug, Clone)]
pub struct ImageArchiveRepo {
    pool: PgPool,
}

impl ImageArchiveRepo {
    /// 新建。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 记录一次归档。
    pub async fn insert(
        &self,
        file_name: &str,
        bytes: i64,
        image_count: i32,
        image_bytes: i64,
    ) -> Result<ImageArchiveRow, StorageError> {
        let row = sqlx::query_as::<_, ImageArchiveRow>(
            r#"insert into image_archives (id, file_name, bytes, image_count, image_bytes)
               values ($1, $2, $3, $4, $5)
               returning id, file_name, bytes, image_count, image_bytes, created_at"#,
        )
        .bind(Uuid::new_v4())
        .bind(file_name)
        .bind(bytes)
        .bind(image_count)
        .bind(image_bytes)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// 最近若干条。
    pub async fn list(&self, limit: i64) -> Result<Vec<ImageArchiveRow>, StorageError> {
        let rows = sqlx::query_as::<_, ImageArchiveRow>(
            r#"select id, file_name, bytes, image_count, image_bytes, created_at
               from image_archives order by created_at desc limit $1"#,
        )
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

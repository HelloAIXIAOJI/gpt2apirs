//! 参考图清单仓储。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 一条参考图记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImageReferenceRow {
    /// 主键。
    pub id: Uuid,
    /// 所属生成图。
    pub image_id: Uuid,
    /// 顺序（从 1 开始）。
    pub ordinal: i32,
    /// 相对数据目录的路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
    /// 记录时间。
    pub created_at: DateTime<Utc>,
}

/// 新参考图。
#[derive(Debug, Clone)]
pub struct NewImageReference {
    /// 所属生成图。
    pub image_id: Uuid,
    /// 顺序。
    pub ordinal: i32,
    /// 路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
}

/// 参考图仓储。
#[derive(Debug, Clone)]
pub struct ImageReferenceRepo {
    pool: PgPool,
}

impl ImageReferenceRepo {
    /// 新建。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 批量写入。
    pub async fn insert_many(&self, items: &[NewImageReference]) -> Result<(), StorageError> {
        for item in items {
            sqlx::query(
                r#"insert into image_references (id, image_id, ordinal, rel_path, mime, bytes)
                   values ($1, $2, $3, $4, $5, $6)"#,
            )
            .bind(Uuid::new_v4())
            .bind(item.image_id)
            .bind(item.ordinal)
            .bind(&item.rel_path)
            .bind(&item.mime)
            .bind(item.bytes)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 全部参考图（归档用）。
    pub async fn list_all(&self) -> Result<Vec<ImageReferenceRow>, StorageError> {
        let rows = sqlx::query_as::<_, ImageReferenceRow>(
            r#"select id, image_id, ordinal, rel_path, mime, bytes, created_at
               from image_references order by image_id, ordinal"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

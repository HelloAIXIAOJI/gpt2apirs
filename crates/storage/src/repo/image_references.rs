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
    /// 相对数据目录的路径（内容寻址：由 sha256 决定）。
    pub rel_path: String,
    /// 内容哈希；旧数据可能为空。
    pub sha256: Option<String>,
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
    /// 内容哈希。
    pub sha256: String,
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
                r#"insert into image_references (id, image_id, ordinal, rel_path, mime, bytes, sha256)
                   values ($1, $2, $3, $4, $5, $6, $7)"#,
            )
            .bind(Uuid::new_v4())
            .bind(item.image_id)
            .bind(item.ordinal)
            .bind(&item.rel_path)
            .bind(&item.mime)
            .bind(item.bytes)
            .bind(&item.sha256)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 某个路径还有多少条记录引用（删文件前用）。
    pub async fn count_by_path(&self, rel_path: &str) -> Result<i64, StorageError> {
        let (count,): (i64,) =
            sqlx::query_as("select count(*) from image_references where rel_path = $1")
                .bind(rel_path)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// 某张图用到的参考图路径。
    pub async fn paths_for_image(&self, image_id: Uuid) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "select rel_path from image_references where image_id = $1",
        )
        .bind(image_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(path,)| path).collect())
    }

    /// 全部仍然被引用的路径（GC 用）。
    pub async fn referenced_paths(&self) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "select distinct rel_path from image_references",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(path,)| path).collect())
    }

    /// 全部参考图（归档用）。
    pub async fn list_all(&self) -> Result<Vec<ImageReferenceRow>, StorageError> {
        let rows = sqlx::query_as::<_, ImageReferenceRow>(
            r#"select id, image_id, ordinal, rel_path, sha256, mime, bytes, created_at
               from image_references order by image_id, ordinal"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

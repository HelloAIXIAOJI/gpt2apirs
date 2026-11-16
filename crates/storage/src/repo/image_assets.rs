//! 图片元数据仓储。
//!
//! 图片字节在文件系统，库表只存索引：后台要按账号/时间/大小查询时，
//! 光靠目录名做不到。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 一条待写入的图片元数据。
#[derive(Debug, Clone)]
pub struct NewImageAsset {
    /// 相对 `DATA_DIR` 的路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
    /// 宽。
    pub width: i32,
    /// 高。
    pub height: i32,
    /// 生成它的账号。
    pub account_id: Option<Uuid>,
    /// 提示词。
    pub prompt: Option<String>,
    /// 生成这张图用的 API key（删除后置空）。
    pub api_key_id: Option<Uuid>,
    /// 生成这张图用的 API key 前缀（key 删除后仍可追溯）。
    pub api_key_prefix: Option<String>,
}

/// 查询到的一条图片记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImageAssetRow {
    /// 主键。
    pub id: Uuid,
    /// 相对路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
    /// 宽。
    pub width: i32,
    /// 高。
    pub height: i32,
    /// 账号。
    pub account_id: Option<Uuid>,
    /// 提示词。
    pub prompt: Option<String>,
    /// 生成这张图用的 API key（删除后为空）。
    pub api_key_id: Option<Uuid>,
    /// 生成这张图用的 API key 前缀。
    pub api_key_prefix: Option<String>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
}

/// 查询条件。
#[derive(Debug, Clone, Default)]
pub struct ImageAssetQuery {
    /// 只看该账号。
    pub account_id: Option<Uuid>,
    /// 起始时间。
    pub since: Option<DateTime<Utc>>,
    /// 条数上限。
    pub limit: i64,
    /// 偏移。
    pub offset: i64,
}

/// 汇总。
#[derive(Debug, Clone, Default)]
pub struct ImageAssetStats {
    /// 张数。
    pub count: i64,
    /// 总字节。
    pub bytes: i64,
}

/// 图片元数据仓储。
#[derive(Debug, Clone)]
pub struct ImageAssetRepo {
    pool: PgPool,
}

impl ImageAssetRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 写入一条（同一路径幂等）。
    pub async fn insert(&self, asset: &NewImageAsset) -> Result<Uuid, StorageError> {
        let (id,): (Uuid,) = sqlx::query_as(SQL_INSERT)
            .bind(Uuid::new_v4())
            .bind(&asset.rel_path)
            .bind(&asset.mime)
            .bind(asset.bytes)
            .bind(asset.width)
            .bind(asset.height)
            .bind(asset.account_id)
            .bind(asset.prompt.as_deref())
            .bind(asset.api_key_id)
            .bind(asset.api_key_prefix.as_deref())
            .fetch_one(&self.pool)
            .await?;
        Ok(id)
    }

    /// 分页查询，按时间倒序。
    pub async fn list(&self, query: &ImageAssetQuery) -> Result<Vec<ImageAssetRow>, StorageError> {
        let rows = sqlx::query_as::<_, ImageAssetRow>(SQL_LIST)
            .bind(query.account_id)
            .bind(query.since)
            .bind(query.limit.clamp(1, 200))
            .bind(query.offset.max(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 汇总。
    pub async fn stats(&self, since: Option<DateTime<Utc>>) -> Result<ImageAssetStats, StorageError> {
        let (count, bytes): (i64, Option<i64>) = sqlx::query_as(SQL_STATS)
            .bind(since)
            .fetch_one(&self.pool)
            .await?;
        Ok(ImageAssetStats {
            count,
            bytes: bytes.unwrap_or(0),
        })
    }

    /// 取一条（删除前需要它的路径）。
    pub async fn get(&self, id: Uuid) -> Result<Option<ImageAssetRow>, StorageError> {
        let row = sqlx::query_as::<_, ImageAssetRow>(SQL_GET)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 删除一条，返回是否命中。
    pub async fn delete(&self, id: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query("delete from image_assets where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 删除早于 N 小时的记录，返回被删记录的相对路径（供调用方删文件）。
    pub async fn purge_older_than(&self, hours: i64) -> Result<Vec<String>, StorageError> {
        let window = format!("{} hours", hours.max(1));
        let rows: Vec<(String,)> = sqlx::query_as(SQL_PURGE)
            .bind(&window)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(path,)| path).collect())
    }
}

const SQL_INSERT: &str = r#"insert into image_assets
    (id, rel_path, mime, bytes, width, height, account_id, prompt, api_key_id, api_key_prefix)
  values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
  on conflict (rel_path) do update set
    mime = excluded.mime, bytes = excluded.bytes, width = excluded.width,
    height = excluded.height, account_id = excluded.account_id, prompt = excluded.prompt,
    api_key_id = excluded.api_key_id, api_key_prefix = excluded.api_key_prefix
  returning id"#;

const SQL_LIST: &str = r#"select id, rel_path, mime, bytes, width, height, account_id, prompt,
       api_key_id, api_key_prefix, created_at
  from image_assets
  where ($1::uuid is null or account_id = $1)
    and ($2::timestamptz is null or created_at >= $2)
  order by created_at desc
  limit $3 offset $4"#;

// sum(bigint) 在 PG 里是 numeric，必须显式转回 bigint，否则解码报类型不匹配
const SQL_STATS: &str = r#"select count(*), coalesce(sum(bytes), 0)::bigint from image_assets
  where ($1::timestamptz is null or created_at >= $1)"#;

const SQL_GET: &str = r#"select id, rel_path, mime, bytes, width, height, account_id, prompt,
       api_key_id, api_key_prefix, created_at
  from image_assets where id = $1"#;

const SQL_PURGE: &str = r#"delete from image_assets
  where created_at < now() - $1::interval
  returning rel_path"#;

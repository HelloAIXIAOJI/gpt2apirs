//! 账号桶仓储。
//!
//! 桶是「一批账号 + 一个出口」的组合：账号只说自己在哪个桶，
//! 出口（节点名 / 自定义地址）挂在桶上，改一次就惠及桶里所有账号。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 默认桶的固定 ID：迁移与脚本直接引用，不用先查。
///
/// 必须与 `0013_buckets.sql` 里写入的值**逐字节一致**，否则「默认桶不能删」
/// 这类判断会静默失效（`from_u128(1)` 得到的是全零版本串，看上去很像但不是同一个）。
pub const DEFAULT_BUCKET_ID: Uuid = Uuid::from_u128(0x4000_8000_0000_0000_0001);

/// 一个桶（带账号数）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BucketRow {
    /// 主键。
    pub id: Uuid,
    /// 名称。
    pub name: String,

    /// 是否默认桶（不能删）。
    pub is_default: bool,
    /// 桶里有多少账号（删除前要判断）。
    pub account_count: i64,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// 桶仓储。
#[derive(Debug, Clone)]
pub struct BucketRepo {
    pool: PgPool,
}

impl BucketRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 全部桶（默认桶排最前，其余按创建时间）。
    pub async fn list(&self) -> Result<Vec<BucketRow>, StorageError> {
        let rows = sqlx::query_as::<_, BucketRow>(&sql_list())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 按 ID 查。
    pub async fn get(&self, id: Uuid) -> Result<Option<BucketRow>, StorageError> {
        let row = sqlx::query_as::<_, BucketRow>(&sql_get())
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 默认桶 ID。
    ///
    /// 理论上库里有且只有一个（有唯一索引兜着）；真查不到就回退到固定 ID，
    /// 让「导入未指定桶」这条路径永远有个确定的落点。
    pub async fn default_id(&self) -> Result<Uuid, StorageError> {
        let found: Option<Uuid> = sqlx::query_scalar(SQL_DEFAULT_ID).fetch_optional(&self.pool).await?;
        Ok(found.unwrap_or(DEFAULT_BUCKET_ID))
    }

    /// 新建桶。返回新桶 ID。
    ///
    /// 名字重复由唯一索引拦下（返回 StorageError），调用方转成「桶名已存在」。
    pub async fn create(&self, name: &str) -> Result<Uuid, StorageError> {
        let id: Uuid = sqlx::query_scalar(SQL_CREATE)
            .bind(name.trim())
            .fetch_one(&self.pool)
            .await?;
        Ok(id)
    }

    /// 改名。返回是否命中。
    pub async fn rename(&self, id: Uuid, name: &str) -> Result<bool, StorageError> {
        let result = sqlx::query(SQL_RENAME)
            .bind(id)
            .bind(name.trim())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }


    /// 删桶。桶里还有账号时返回 `Err(BucketBusy)`。
    pub async fn delete(&self, id: Uuid) -> Result<bool, StorageError> {
        let count: i64 = sqlx::query_scalar(SQL_COUNT_ACCOUNTS)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        if count > 0 {
            return Err(StorageError::BucketBusy { accounts: count });
        }
        let result = sqlx::query(SQL_DELETE).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// 桶里有多少账号。
    pub async fn account_count(&self, id: Uuid) -> Result<i64, StorageError> {
        let count: i64 = sqlx::query_scalar(SQL_COUNT_ACCOUNTS)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }
}

const SELECT_COLUMNS: &str = "b.id, b.name, b.is_default, \
     (select count(*) from accounts a where a.bucket_id = b.id) as account_count, \
     b.created_at, b.updated_at";

fn sql_list() -> String {
    format!("select {SELECT_COLUMNS} from buckets b order by b.is_default desc, b.created_at")
}

fn sql_get() -> String {
    format!("select {SELECT_COLUMNS} from buckets b where b.id = $1")
}

const SQL_DEFAULT_ID: &str = "select id from buckets where is_default limit 1";
const SQL_CREATE: &str = "insert into buckets (id, name) values (gen_random_uuid(), $1) returning id";
const SQL_RENAME: &str = "update buckets set name = $2, updated_at = now() where id = $1";

const SQL_DELETE: &str = "delete from buckets where id = $1 and not is_default";
const SQL_COUNT_ACCOUNTS: &str = "select count(*) from accounts where bucket_id = $1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bucket_id_is_stable() {
        // 迁移里写死的默认桶 ID 必须与代码一致：对不上就会「导入后找不到桶」
        assert_eq!(
            DEFAULT_BUCKET_ID.to_string(),
            "00000000-0000-4000-8000-000000000001"
        );
    }
}

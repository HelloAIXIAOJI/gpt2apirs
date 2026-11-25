//! API key 仓储。
//!
//! 库里只存 sha256；明文只在 `create` 的返回值里出现一次。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 一把 key 的对外信息（不含明文）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyRow {
    /// 主键。
    pub id: Uuid,
    /// 人类可读名称。
    pub name: String,
    /// 展示前缀。
    pub prefix: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 最近一次使用时间。
    pub last_used_at: Option<DateTime<Utc>>,
    /// 吊销时间；非空即失效。
    pub revoked_at: Option<DateTime<Utc>>,
    /// 绑定的桶；`None` = 全局（任何桶的账号都能用）。
    pub bucket_id: Option<Uuid>,
}

/// 新建结果：记录 + 只出现一次的明文。
#[derive(Debug, Clone)]
pub struct CreatedApiKey {
    /// 记录。
    pub row: ApiKeyRow,
    /// 明文。
    pub plaintext: String,
}

/// API key 仓储。
#[derive(Debug, Clone)]
pub struct ApiKeyRepo {
    pool: PgPool,
}

impl ApiKeyRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 新建一把 key（可绑定桶；`None` = 全局）。
    pub async fn create(
        &self,
        name: &str,
        bucket_id: Option<Uuid>,
    ) -> Result<CreatedApiKey, StorageError> {
        let generated = gpt2apirs_core::api_key::generate();
        let id = Uuid::new_v4();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"insert into api_keys (id, name, key_sha256, prefix, bucket_id)
               values ($1, $2, $3, $4, $5)
               returning id, name, prefix, created_at, last_used_at, revoked_at, bucket_id"#,
        )
        .bind(id)
        .bind(name.trim())
        .bind(&generated.sha256)
        .bind(&generated.prefix)
        .bind(bucket_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(CreatedApiKey {
            row,
            plaintext: generated.plaintext,
        })
    }

    /// 全部 key（含已吊销）。
    pub async fn list(&self) -> Result<Vec<ApiKeyRow>, StorageError> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            r#"select id, name, prefix, created_at, last_used_at, revoked_at, bucket_id
               from api_keys order by created_at desc"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 吊销。返回是否命中了未吊销的记录。
    pub async fn revoke(&self, id: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "update api_keys set revoked_at = now() where id = $1 and revoked_at is null",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 改名。返回是否命中。
    pub async fn rename(&self, id: Uuid, name: &str) -> Result<bool, StorageError> {
        let result = sqlx::query("update api_keys set name = $2 where id = $1")
            .bind(id)
            .bind(name.trim())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 编辑：名称与所属桶。
    ///
    /// `bucket_id` 用 `Option<Option<Uuid>>` 表达三态：
    /// `None` = 不动，`Some(None)` = 改成全局，`Some(Some(id))` = 改到该桶。
    pub async fn update(
        &self,
        id: Uuid,
        name: Option<&str>,
        bucket_id: Option<Option<Uuid>>,
    ) -> Result<bool, StorageError> {
        // 分成两条 update：`case when $3 then $4 ...` 里参数类型得让 Postgres 猜，
        // 猜错时静默不生效（踩过：改名生效、换桶没生效）。分开写没有这层不确定。
        let mut touched = false;
        if let Some(name) = name {
            let result = sqlx::query("update api_keys set name = $2 where id = $1")
                .bind(id)
                .bind(name.trim())
                .execute(&self.pool)
                .await?;
            touched = result.rows_affected() > 0;
        }
        if let Some(value) = bucket_id {
            let result = sqlx::query("update api_keys set bucket_id = $2 where id = $1")
                .bind(id)
                .bind(value)
                .execute(&self.pool)
                .await?;
            touched = result.rows_affected() > 0 || touched;
        }
        Ok(touched)
    }

    /// 恢复一把已吊销的 key（明文不变，`key_sha256` 也不动）。
    ///
    /// 返回 `false` 表示没这把 key；本来就是可用的 key 会返回 `true` 但不是「恢复」语义，
    /// 所以调用方按「恢复后是否可用」来判断，不按状态变化判断。
    pub async fn restore(&self, id: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query("update api_keys set revoked_at = null where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 物理删除。返回是否删掉了记录。
    ///
    /// 与 `revoke` 分开：吊销只是标记失效、记录还在；删除是不可逆的清理动作。
    pub async fn delete(&self, id: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query("delete from api_keys where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 校验一把明文 key。命中则刷新 `last_used_at`。
    pub async fn verify(&self, plaintext: &str) -> Result<Option<ApiKeyRow>, StorageError> {
        let digest = gpt2apirs_core::api_key::hash(plaintext.trim());
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"update api_keys set last_used_at = now()
               where key_sha256 = $1 and revoked_at is null
               returning id, name, prefix, created_at, last_used_at, revoked_at, bucket_id"#,
        )
        .bind(&digest)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// 未吊销的数量。
    pub async fn active_count(&self) -> Result<i64, StorageError> {
        let (count,): (i64,) =
            sqlx::query_as("select count(*) from api_keys where revoked_at is null")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}

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

    /// 新建一把 key。
    pub async fn create(&self, name: &str) -> Result<CreatedApiKey, StorageError> {
        let generated = gpt2apirs_core::api_key::generate();
        let id = Uuid::new_v4();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"insert into api_keys (id, name, key_sha256, prefix)
               values ($1, $2, $3, $4)
               returning id, name, prefix, created_at, last_used_at, revoked_at"#,
        )
        .bind(id)
        .bind(name.trim())
        .bind(&generated.sha256)
        .bind(&generated.prefix)
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
            r#"select id, name, prefix, created_at, last_used_at, revoked_at
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

    /// 校验一把明文 key。命中则刷新 `last_used_at`。
    pub async fn verify(&self, plaintext: &str) -> Result<Option<ApiKeyRow>, StorageError> {
        let digest = gpt2apirs_core::api_key::hash(plaintext.trim());
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"update api_keys set last_used_at = now()
               where key_sha256 = $1 and revoked_at is null
               returning id, name, prefix, created_at, last_used_at, revoked_at"#,
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

    /// 把旧配置里的 `AUTH_KEY` 迁成一把名为 `legacy` 的 key。
    ///
    /// 只在库里一把 key 都没有时执行，避免反复写入。返回是否真的写了。
    pub async fn import_legacy(&self, plaintext: &str) -> Result<bool, StorageError> {
        if plaintext.trim().is_empty() || self.active_count().await? > 0 {
            return Ok(false);
        }
        let digest = gpt2apirs_core::api_key::hash(plaintext.trim());
        let prefix: String = plaintext.trim().chars().take(16).collect();
        sqlx::query(
            "insert into api_keys (id, name, key_sha256, prefix) values ($1, 'legacy', $2, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(&digest)
        .bind(&prefix)
        .execute(&self.pool)
        .await?;
        Ok(true)
    }
}

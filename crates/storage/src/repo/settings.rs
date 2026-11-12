//! 运行层设置的持久化。
//!
//! 只有一种后端（PostgreSQL），且集成测试直接打真库。
//! 抽 trait 只会增加一层间接、降低可读性。
//! 等出现第二个后端或必须 mock 时再抽——那时才知道该抽什么。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::pool::StorageError;

/// 咨询锁的键名。
const ADVISORY_LOCK_KEY: &str = "system_settings";

/// 一次读取得到的结果。
#[derive(Debug, Clone)]
pub struct SettingsSnapshot {
    /// 设置内容。
    pub data: HashMap<String, Value>,
    /// 最近更新时间。
    pub updated_at: Option<DateTime<Utc>>,
}

/// PostgreSQL 设置仓储。
#[derive(Debug, Clone)]
pub struct PgSettingsRepo {
    pool: PgPool,
}

impl PgSettingsRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 读取全部设置（不加锁）。
    pub async fn load(&self) -> Result<SettingsSnapshot, StorageError> {
        let row: Option<(Value, DateTime<Utc>)> =
            sqlx::query_as("select data, updated_at from system_settings where id = 1")
                .fetch_optional(&self.pool)
                .await?;

        Ok(match row {
            Some((data, updated_at)) => SettingsSnapshot {
                data: json_object_to_map(data),
                updated_at: Some(updated_at),
            },
            // 表里没有行（理论上迁移会插入，但容错）
            None => SettingsSnapshot {
                data: HashMap::new(),
                updated_at: None,
            },
        })
    }

    /// 最近更新时间（用于「变了才重载」）。
    pub async fn last_updated(&self) -> Result<Option<DateTime<Utc>>, StorageError> {
        let row: Option<(DateTime<Utc>,)> =
            sqlx::query_as("select updated_at from system_settings where id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(at,)| at))
    }

    /// **整体替换**设置内容。
    pub async fn replace(
        &self,
        data: &HashMap<String, Value>,
    ) -> Result<DateTime<Utc>, StorageError> {
        let payload = Value::Object(
            data.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );

        let mut tx = self.pool.begin().await?;
        acquire_advisory_lock(&mut tx).await?;

        let updated_at: (DateTime<Utc>,) = sqlx::query_as(
            "update system_settings set data = $1, updated_at = now() where id = 1 \
             returning updated_at",
        )
        .bind(&payload)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(updated_at.0)
    }

    /// **合并**（部分更新）：只覆盖传入的键，其余保留。
    pub async fn merge(
        &self,
        patch: &HashMap<String, Value>,
    ) -> Result<SettingsSnapshot, StorageError> {
        let mut tx = self.pool.begin().await?;
        acquire_advisory_lock(&mut tx).await?;

        // 行锁读当前值
        let current: Option<(Value,)> =
            sqlx::query_as("select data from system_settings where id = 1 for update")
                .fetch_optional(&mut *tx)
                .await?;

        let mut merged = current
            .map(|(data,)| json_object_to_map(data))
            .unwrap_or_default();
        for (key, value) in patch {
            merged.insert(key.clone(), value.clone());
        }

        let payload = Value::Object(
            merged
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );

        let updated_at: (DateTime<Utc>,) = sqlx::query_as(
            "update system_settings set data = $1, updated_at = now() where id = 1 \
             returning updated_at",
        )
        .bind(&payload)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(SettingsSnapshot {
            data: merged,
            updated_at: Some(updated_at.0),
        })
    }

    /// 删除某个键。
    pub async fn remove(&self, key: &str) -> Result<SettingsSnapshot, StorageError> {
        let mut tx = self.pool.begin().await?;
        acquire_advisory_lock(&mut tx).await?;

        let current: Option<(Value,)> =
            sqlx::query_as("select data from system_settings where id = 1 for update")
                .fetch_optional(&mut *tx)
                .await?;

        let mut merged = current
            .map(|(data,)| json_object_to_map(data))
            .unwrap_or_default();
        merged.remove(key);

        let payload = Value::Object(
            merged
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );

        let updated_at: (DateTime<Utc>,) = sqlx::query_as(
            "update system_settings set data = $1, updated_at = now() where id = 1 \
             returning updated_at",
        )
        .bind(&payload)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(SettingsSnapshot {
            data: merged,
            updated_at: Some(updated_at.0),
        })
    }
}

async fn acquire_advisory_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), StorageError> {
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// JSONB 值转 `HashMap`；非对象时返回空 map。
fn json_object_to_map(value: Value) -> HashMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    }
}

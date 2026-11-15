//! API key 存储的两种后端。
//!
//! 生产用 PostgreSQL；内存版用于测试与临时部署 —— 接口完全一致，
//! 不存在「测试走另一条假路径」的情况。

use std::sync::{Arc, RwLock};

use chrono::Utc;
use uuid::Uuid;

use crate::pool::StorageError;
use crate::repo::api_keys::{ApiKeyRepo, ApiKeyRow, CreatedApiKey};

/// API key 存储。
#[derive(Debug, Clone)]
pub enum ApiKeyStore {
    /// 落库。
    Postgres(ApiKeyRepo),
    /// 进程内。重启即丢，适合单测与临时部署。
    Memory(MemoryApiKeys),
}

impl ApiKeyStore {
    /// 新建一把 key。
    pub async fn create(&self, name: &str) -> Result<CreatedApiKey, StorageError> {
        match self {
            Self::Postgres(repo) => repo.create(name).await,
            Self::Memory(memory) => Ok(memory.create(name)),
        }
    }

    /// 全部 key。
    pub async fn list(&self) -> Result<Vec<ApiKeyRow>, StorageError> {
        match self {
            Self::Postgres(repo) => repo.list().await,
            Self::Memory(memory) => Ok(memory.list()),
        }
    }

    /// 吊销。
    pub async fn revoke(&self, id: Uuid) -> Result<bool, StorageError> {
        match self {
            Self::Postgres(repo) => repo.revoke(id).await,
            Self::Memory(memory) => Ok(memory.revoke(id)),
        }
    }

    /// 校验明文；命中即刷新使用时间。
    pub async fn verify(&self, plaintext: &str) -> Result<Option<ApiKeyRow>, StorageError> {
        match self {
            Self::Postgres(repo) => repo.verify(plaintext).await,
            Self::Memory(memory) => Ok(memory.verify(plaintext)),
        }
    }

    /// 未吊销数量。
    pub async fn active_count(&self) -> Result<i64, StorageError> {
        match self {
            Self::Postgres(repo) => repo.active_count().await,
            Self::Memory(memory) => Ok(memory.active_count()),
        }
    }

}

/// 进程内实现。
#[derive(Debug, Clone, Default)]
pub struct MemoryApiKeys {
    /// (记录, sha256)
    inner: Arc<RwLock<Vec<(ApiKeyRow, String)>>>,
}

impl MemoryApiKeys {
    /// 空存储。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一把已知明文的 key（测试与临时部署用）。
    pub fn with_key(self, plaintext: &str, name: &str) -> Self {
        self.insert_named(plaintext, name);
        self
    }

    fn insert_named(&self, plaintext: &str, name: &str) {
        let digest = gpt2apirs_core::api_key::hash(plaintext.trim());
        let prefix: String = plaintext.trim().chars().take(16).collect();
        let mut guard = self.inner.write().expect("锁");
        guard.push((
            ApiKeyRow {
                id: Uuid::new_v4(),
                name: name.to_string(),
                prefix,
                created_at: Utc::now(),
                last_used_at: None,
                revoked_at: None,
            },
            digest,
        ));
    }

    fn create(&self, name: &str) -> CreatedApiKey {
        let generated = gpt2apirs_core::api_key::generate();
        let row = ApiKeyRow {
            id: Uuid::new_v4(),
            name: name.trim().to_string(),
            prefix: generated.prefix.clone(),
            created_at: Utc::now(),
            last_used_at: None,
            revoked_at: None,
        };
        self.inner
            .write()
            .expect("锁")
            .push((row.clone(), generated.sha256));
        CreatedApiKey {
            row,
            plaintext: generated.plaintext,
        }
    }

    fn list(&self) -> Vec<ApiKeyRow> {
        let mut rows: Vec<ApiKeyRow> = self
            .inner
            .read()
            .expect("锁")
            .iter()
            .map(|(row, _)| row.clone())
            .collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        rows
    }

    fn revoke(&self, id: Uuid) -> bool {
        let mut guard = self.inner.write().expect("锁");
        for (row, _) in guard.iter_mut() {
            if row.id == id && row.revoked_at.is_none() {
                row.revoked_at = Some(Utc::now());
                return true;
            }
        }
        false
    }

    fn verify(&self, plaintext: &str) -> Option<ApiKeyRow> {
        let digest = gpt2apirs_core::api_key::hash(plaintext.trim());
        let mut guard = self.inner.write().expect("锁");
        for (row, stored) in guard.iter_mut() {
            if *stored == digest && row.revoked_at.is_none() {
                row.last_used_at = Some(Utc::now());
                return Some(row.clone());
            }
        }
        None
    }

    fn active_count(&self) -> i64 {
        self.inner
            .read()
            .expect("锁")
            .iter()
            .filter(|(row, _)| row.revoked_at.is_none())
            .count() as i64
    }


}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ApiKeyStore {
        ApiKeyStore::Memory(MemoryApiKeys::new())
    }

    #[tokio::test]
    async fn memory_backend_create_verify_revoke() {
        let store = store();
        let created = store.create("测试").await.expect("创建");
        let verified = store.verify(&created.plaintext).await.expect("校验");
        assert_eq!(verified.expect("应命中").name, "测试");

        assert!(store.revoke(created.row.id).await.expect("吊销"));
        assert!(store.verify(&created.plaintext).await.expect("校验").is_none());
        assert_eq!(store.active_count().await.expect("计数"), 0);
    }

    #[tokio::test]
    async fn memory_backend_rejects_wrong_shape() {
        let store = store();
        assert!(store.verify("nope").await.expect("校验").is_none());
    }


}

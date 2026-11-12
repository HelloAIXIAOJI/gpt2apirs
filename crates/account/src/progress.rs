//! 批量操作进度。
//!
//! 内存态，进程重启即丢失 —— 进度只用于前端轮询展示，没有持久化价值。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use uuid::Uuid;

use crate::consts::PROGRESS_RETENTION_SECS;

/// 操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// 同步账号资料与配额。
    Sync,
    /// 只刷新 access token。
    RefreshToken,
    /// 账号测试。
    Test,
    /// 导入。
    Import,
}

/// 单条失败记录。
#[derive(Debug, Clone, Serialize)]
pub struct OperationError {
    /// 账号 ID。
    pub account_id: Uuid,
    /// 账号标识（邮箱或 ID）。
    pub label: String,
    /// 失败原因（已脱敏）。
    pub message: String,
}

/// 一次批量操作的进度。
#[derive(Debug, Clone, Serialize)]
pub struct OperationProgress {
    /// 操作 ID。
    pub id: Uuid,
    /// 操作类型。
    pub kind: OperationKind,
    /// 总数。
    pub total: usize,
    /// 已处理数。
    pub processed: usize,
    /// 是否结束。
    pub done: bool,
    /// 成功数。
    pub succeeded: usize,
    /// 失败数。
    pub failed: usize,
    /// 失败明细。
    pub errors: Vec<OperationError>,
    /// 开始时间。
    pub started_at: DateTime<Utc>,
    /// 结束时间。
    pub finished_at: Option<DateTime<Utc>>,
}

/// 进度存储。
#[derive(Debug, Default)]
pub struct ProgressStore {
    inner: DashMap<Uuid, Arc<std::sync::Mutex<OperationProgress>>>,
}

impl ProgressStore {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 开一次操作，返回操作 ID。
    pub fn start(&self, kind: OperationKind, total: usize) -> Uuid {
        let id = Uuid::new_v4();
        let progress = OperationProgress {
            id,
            kind,
            total,
            processed: 0,
            done: false,
            succeeded: 0,
            failed: 0,
            errors: Vec::new(),
            started_at: Utc::now(),
            finished_at: None,
        };
        self.inner
            .insert(id, Arc::new(std::sync::Mutex::new(progress)));
        id
    }

    /// 记一次成功。
    pub fn record_success(&self, id: Uuid) {
        self.with(id, |progress| {
            progress.processed += 1;
            progress.succeeded += 1;
        });
    }

    /// 记一次失败。
    pub fn record_failure(&self, id: Uuid, account_id: Uuid, label: String, message: String) {
        self.with(id, |progress| {
            progress.processed += 1;
            progress.failed += 1;
            progress.errors.push(OperationError {
                account_id,
                label,
                message,
            });
        });
    }

    /// 结束。
    pub fn finish(&self, id: Uuid) {
        self.with(id, |progress| {
            progress.done = true;
            progress.finished_at = Some(Utc::now());
        });
    }

    /// 读取快照。
    pub fn get(&self, id: Uuid) -> Option<OperationProgress> {
        self.inner.get(&id).and_then(|entry| {
            entry
                .lock()
                .ok()
                .map(|guard| guard.clone())
        })
    }

    /// 清理已结束且超期的记录。
    pub fn prune(&self) {
        let now = Utc::now();
        let stale: Vec<Uuid> = self
            .inner
            .iter()
            .filter_map(|entry| {
                let progress = entry.value().lock().ok()?;
                let finished = progress.finished_at?;
                if (now - finished).num_seconds() > PROGRESS_RETENTION_SECS {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();
        for id in stale {
            self.inner.remove(&id);
        }
    }

    fn with(&self, id: Uuid, mutate: impl FnOnce(&mut OperationProgress)) {
        if let Some(entry) = self.inner.get(&id) {
            if let Ok(mut guard) = entry.lock() {
                mutate(&mut guard);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_then_finish() {
        let store = ProgressStore::new();
        let id = store.start(OperationKind::Sync, 3);
        let snapshot = store.get(id).expect("应存在");
        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.processed, 0);
        assert!(!snapshot.done);

        store.record_success(id);
        store.record_failure(id, Uuid::new_v4(), "a@b".to_string(), "boom".to_string());
        store.finish(id);

        let snapshot = store.get(id).expect("应存在");
        assert_eq!(snapshot.processed, 2);
        assert_eq!(snapshot.succeeded, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.errors.len(), 1);
        assert_eq!(snapshot.errors[0].label, "a@b");
        assert!(snapshot.done);
        assert!(snapshot.finished_at.is_some());
    }

    #[test]
    fn unknown_id_is_noop() {
        let store = ProgressStore::new();
        let ghost = Uuid::new_v4();
        store.record_success(ghost);
        store.finish(ghost);
        assert!(store.get(ghost).is_none());
    }

    #[test]
    fn prune_keeps_running_operations() {
        let store = ProgressStore::new();
        let running = store.start(OperationKind::Import, 10);
        store.prune();
        assert!(store.get(running).is_some(), "未结束的不应被清理");
    }
}

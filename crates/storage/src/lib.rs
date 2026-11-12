//! 存储层：PostgreSQL 连接池、迁移、仓储。
//!
//! **只支持 PostgreSQL**（ADR-008 修订：放弃 SQLite 双后端）。
//!
//! 设计见 `docs/m1-design.md` 第六章。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod backup;
pub mod migrate;
pub mod pool;
pub mod repo;

pub use migrate::{applied_migrations, run_migrations};
pub use pool::{PoolStats, StorageError, build_pool, health_check, pool_stats};
pub use repo::{PgSettingsRepo, SettingsSnapshot};

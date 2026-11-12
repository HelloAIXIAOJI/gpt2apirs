//! 账号池。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod consts;
pub mod credentials;
pub mod export;
pub mod import;
pub mod model;
pub mod oauth;
pub mod pool;
pub mod progress;
pub mod refresh;
pub mod remote;
pub mod repo;
pub mod sync;
pub mod watcher;

pub use credentials::{
    AccessTokenStatus, Availability, CredentialAvailability, RefreshTokenStatus, project,
};
pub use export::{ExportItem, ExportReport, build_export, build_token_txt};
pub use import::{ImportParseError, parse_any, parse_json, parse_token_lines};
pub use model::{Account, AccountStatus, Plan, Source};
pub use oauth::{OAuthError, OAuthSessions, OAuthStart, SharedSessions};
pub use pool::{
    AccountLease, AccountPool, InvalidOutcome, PoolConfig, PoolError, Purpose, SelectionFilter,
    should_defer_invalid,
};
pub use progress::{OperationError, OperationKind, OperationProgress, ProgressStore};
pub use refresh::{RefreshError, RefreshOutcome, refresh_tokens};
pub use remote::{RemoteError, RemoteInfo, fetch_remote_info};
pub use repo::{
    AccountError, AccountFilter, AccountPatch, AccountPoolStats, AccountRepo, NewAccount,
    UpsertOutcome, UpsertReport,
};
pub use sync::{
    refresh_account_token, run_refresh_batch, run_sync_batch, spawn_refresh_batch,
    spawn_sync_batch, sync_account,
};
pub use watcher::{WatcherConfig, WatcherRound, run_round, spawn_watcher};

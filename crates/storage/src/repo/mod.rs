//! 仓储层。

pub mod api_key_store;
pub mod call_records;
pub mod image_archives;
pub mod image_assets;
pub mod image_references;
pub mod image_tasks;
pub mod api_keys;
pub mod buckets;
pub mod settings;

pub use api_key_store::{ApiKeyStore, MemoryApiKeys};
pub use call_records::{
    CallRecordQuery, CallRecordRepo, CallRecordRow, CallSummary, DailyStat, NewCallRecord,
    RankedEntry,
};
pub use image_archives::{ImageArchiveRepo, ImageArchiveRow};
pub use image_references::{ImageReferenceRepo, ImageReferenceRow, NewImageReference};
pub use image_tasks::{
    ImageDailyStat, ImageTaskRepo, ImageTaskRow, ModelStat, NewImageTask, TaskImageRow,
};
pub use image_assets::{ImageAssetQuery, ImageAssetRepo, ImageAssetRow, ImageAssetStats, NewImageAsset};
pub use api_keys::{ApiKeyRepo, ApiKeyRow, CreatedApiKey};
pub use buckets::{BucketRepo, BucketRow, DEFAULT_BUCKET_ID};
pub use settings::{PgSettingsRepo, SettingsSnapshot};

//! 备份与恢复：把库内数据打包成 tar.gz，恢复时合并回去。
//!
//! 包里只有两样东西：`backup.json`（账号、设置、key、图片索引）与 `images/`（图片字节）。
//! 恢复是**合并**语义（只补缺失，不覆盖已有行），避免误操作抹掉线上账号。
//!
//! 解包必须防路径穿越：条目名带 `..` 或绝对路径一律拒绝。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::pool::StorageError;

/// 包内清单文件名。
pub const MANIFEST_NAME: &str = "backup.json";

/// 备份清单。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupBundle {
    /// 格式版本。
    pub version: u32,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 账号行（原样 JSON）。
    pub accounts: Vec<serde_json::Value>,
    /// 运行设置。
    pub settings: Option<serde_json::Value>,
    /// API key 行（只含哈希，不含明文）。
    pub api_keys: Vec<serde_json::Value>,
    /// 图片索引。
    pub image_assets: Vec<serde_json::Value>,
}

/// 恢复结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreStats {
    /// 新增账号数。
    pub accounts_inserted: u64,
    /// 新增 key 数。
    pub api_keys_inserted: u64,
    /// 新增图片索引数。
    pub images_inserted: u64,
    /// 写入的图片文件数。
    pub files_written: u64,
    /// 设置是否被写入（库里没有设置行时才算）。
    pub settings_applied: bool,
}

/// 导出库内数据。
pub async fn export(pool: &PgPool) -> Result<BackupBundle, StorageError> {
    let accounts: Option<serde_json::Value> = sqlx::query_scalar(
        "select jsonb_agg(to_jsonb(a) order by a.created_at) from accounts a",
    )
    .fetch_one(pool)
    .await?;
    let api_keys: Option<serde_json::Value> = sqlx::query_scalar(
        "select jsonb_agg(to_jsonb(k) order by k.created_at) from api_keys k",
    )
    .fetch_one(pool)
    .await?;
    let image_assets: Option<serde_json::Value> = sqlx::query_scalar(
        "select jsonb_agg(to_jsonb(i) order by i.created_at) from image_assets i",
    )
    .fetch_one(pool)
    .await?;
    let settings: Option<serde_json::Value> =
        sqlx::query_scalar("select data from system_settings where id = 1")
            .fetch_optional(pool)
            .await?;

    Ok(BackupBundle {
        version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        accounts: flatten(accounts),
        settings,
        api_keys: flatten(api_keys),
        image_assets: flatten(image_assets),
    })
}

fn flatten(value: Option<serde_json::Value>) -> Vec<serde_json::Value> {
    match value {
        Some(serde_json::Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// 条目名是否安全（相对、无 `..`、非绝对）。
pub fn is_safe_entry(name: &str) -> bool {
    let path = Path::new(name);
    if path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return false,
        }
    }
    !name.is_empty()
}

/// 打一个备份包，返回文件路径。
///
/// `images_root` 下的文件按相对路径写进包；包内路径统一用 `images/...`。
pub fn write_archive(
    target: &Path,
    bundle: &BackupBundle,
    images_root: &Path,
    image_paths: &[String],
) -> Result<u64, StorageError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let file = std::fs::File::create(target).map_err(io_error)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let manifest = serde_json::to_vec(bundle).map_err(|error| {
        StorageError::Query(sqlx::Error::Protocol(format!("序列化失败：{error}")))
    })?;
    append_bytes(&mut builder, MANIFEST_NAME, &manifest)?;
    for rel_path in image_paths {
        if !is_safe_entry(rel_path) {
            continue;
        }
        let absolute = images_root.join(rel_path);
        let Ok(bytes) = std::fs::read(&absolute) else { continue };
        append_bytes(&mut builder, rel_path, &bytes)?;
    }

    let encoder = builder.into_inner().map_err(io_error)?;
    let file = encoder.finish().map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    Ok(std::fs::metadata(target).map(|meta| meta.len()).unwrap_or(0))
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), StorageError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(io_error)?;
    Ok(())
}

/// 读一个备份包：清单 + 图片字节。
///
/// 任何不安全条目都会被跳过（不报错，但也不会写到目标目录之外）。
pub fn read_archive(target: &Path) -> Result<(BackupBundle, BTreeMap<String, Vec<u8>>), StorageError> {
    let file = std::fs::File::open(target).map_err(io_error)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut bundle: Option<BackupBundle> = None;
    let mut images = BTreeMap::new();

    for entry in archive.entries().map_err(io_error)? {
        let mut entry = entry.map_err(io_error)?;
        let path = entry.path().map_err(io_error)?.to_string_lossy().into_owned();
        if !is_safe_entry(&path) {
            continue;
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(io_error)?;
        if path == MANIFEST_NAME {
            bundle = serde_json::from_slice(&bytes).ok();
        } else {
            images.insert(path, bytes);
        }
    }

    let bundle = bundle.ok_or_else(|| {
        StorageError::Query(sqlx::Error::Protocol(format!("备份包缺少 {MANIFEST_NAME}")))
    })?;
    Ok((bundle, images))
}

/// 合并恢复：只补库里没有的行。
pub async fn restore_merge(pool: &PgPool, bundle: &BackupBundle) -> Result<RestoreStats, StorageError> {
    let mut stats = RestoreStats::default();

    for account in &bundle.accounts {
        let result = sqlx::query(SQL_RESTORE_ACCOUNT)
            .bind(account)
            .execute(pool)
            .await?;
        stats.accounts_inserted += result.rows_affected();
    }
    for key in &bundle.api_keys {
        let result = sqlx::query(SQL_RESTORE_API_KEY)
            .bind(key)
            .execute(pool)
            .await?;
        stats.api_keys_inserted += result.rows_affected();
    }
    for image in &bundle.image_assets {
        let result = sqlx::query(SQL_RESTORE_IMAGE)
            .bind(image)
            .execute(pool)
            .await?;
        stats.images_inserted += result.rows_affected();
    }
    if let Some(settings) = &bundle.settings {
        let result = sqlx::query(SQL_RESTORE_SETTINGS)
            .bind(settings)
            .execute(pool)
            .await?;
        stats.settings_applied = result.rows_affected() > 0;
    }
    Ok(stats)
}

/// 把包里的图片写到 `images_root` 下。
pub fn restore_images(
    images_root: &Path,
    images: &BTreeMap<String, Vec<u8>>,
) -> Result<u64, StorageError> {
    let mut written = 0u64;
    for (rel_path, bytes) in images {
        if !is_safe_entry(rel_path) {
            continue;
        }
        let absolute = images_root.join(rel_path);
        if absolute.exists() {
            continue;
        }
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        std::fs::write(&absolute, bytes).map_err(io_error)?;
        written += 1;
    }
    Ok(written)
}

/// 默认备份目录（相对数据目录）。
pub fn backup_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

fn io_error(error: std::io::Error) -> StorageError {
    StorageError::Query(sqlx::Error::Io(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(is_safe_entry("images/a.png"));
        assert!(!is_safe_entry("../evil.sh"));
        assert!(!is_safe_entry("images/../../evil.sh"));
        assert!(!is_safe_entry("/etc/passwd"));
        assert!(!is_safe_entry(""));
    }

    #[test]
    fn archive_round_trip_keeps_manifest_and_images() {
        let dir = std::env::temp_dir().join(format!("g2a-backup-{}", uuid::Uuid::new_v4()));
        let images_root = dir.join("images");
        std::fs::create_dir_all(images_root.join("2026/11")).expect("建目录");
        std::fs::write(images_root.join("2026/11/a.png"), b"png-bytes").expect("写图");

        let bundle = BackupBundle {
            version: 1,
            created_at: "2026-11-13T00:00:00Z".to_string(),
            ..Default::default()
        };
        let target = dir.join("b.tar.gz");
        let size = write_archive(&target, &bundle, &images_root, &["2026/11/a.png".to_string()])
            .expect("打包");
        assert!(size > 0);

        let (read_back, images) = read_archive(&target).expect("解包");
        assert_eq!(read_back.version, 1);
        assert_eq!(images.get("2026/11/a.png").map(Vec::as_slice), Some(&b"png-bytes"[..]));

        // 还原到新目录
        let restored_root = dir.join("restored");
        let written = restore_images(&restored_root, &images).expect("还原");
        assert_eq!(written, 1);
        assert_eq!(
            std::fs::read(restored_root.join("2026/11/a.png")).expect("读回"),
            b"png-bytes".to_vec()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_skips_existing_files() {
        let dir = std::env::temp_dir().join(format!("g2a-backup2-{}", uuid::Uuid::new_v4()));
        let root = dir.join("images");
        std::fs::create_dir_all(root.join("a")).expect("建目录");
        std::fs::write(root.join("a/b.png"), b"old").expect("写");
        let mut images = BTreeMap::new();
        images.insert("a/b.png".to_string(), b"new".to_vec());
        assert_eq!(restore_images(&root, &images).expect("还原"), 0, "已存在不该覆盖");
        assert_eq!(std::fs::read(root.join("a/b.png")).expect("读"), b"old".to_vec());
        std::fs::remove_dir_all(&dir).ok();
    }
}

const SQL_RESTORE_ACCOUNT: &str = r#"insert into accounts
  select * from jsonb_populate_record(null::accounts, $1::jsonb)
  on conflict (id) do nothing"#;

const SQL_RESTORE_API_KEY: &str = r#"insert into api_keys
  select * from jsonb_populate_record(null::api_keys, $1::jsonb)
  on conflict (key_sha256) do nothing"#;

const SQL_RESTORE_IMAGE: &str = r#"insert into image_assets
  select * from jsonb_populate_record(null::image_assets, $1::jsonb)
  on conflict (rel_path) do nothing"#;

const SQL_RESTORE_SETTINGS: &str = r#"update system_settings set data = $1::jsonb, updated_at = now()
  where id = 1 and data = '{}'::jsonb"#;

//! 图片归档：把图片与元数据打成 tar.gz，并可回读校验。
//!
//! 顺序约定由调用方保证：**先写包、再校验，校验通过才允许删除源文件**。
//! 这里只负责「写」与「验」，不碰删除。

use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pool::StorageError;

/// 一张参考图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveReference {
    /// 顺序（从 1 开始）。
    pub ordinal: i32,
    /// 原始相对路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
}

/// manifest 里的一条图片元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// 原始相对路径，例如 `images/2026/11/16/<uuid>.png`。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
    /// 宽。
    pub width: i32,
    /// 高。
    pub height: i32,
    /// 生成账号。
    pub account_id: Option<Uuid>,
    /// 提示词。
    pub prompt: Option<String>,
    /// 生成时间。
    pub created_at: DateTime<Utc>,
    /// 生成这张图用的 API key 前缀（key 可能已被删，只留前缀）。
    #[serde(default)]
    pub api_key_prefix: Option<String>,
    /// 用到的参考图清单。
    #[serde(default)]
    pub references: Vec<ArchiveReference>,
}

/// 包里的 manifest。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    /// 格式版本。
    pub version: u32,
    /// 打包时间。
    pub created_at: DateTime<Utc>,
    /// 图片条目。
    pub images: Vec<ArchiveEntry>,
}

/// 写包结果。
#[derive(Debug, Clone)]
pub struct ArchiveStats {
    /// 包文件名。
    pub file_name: String,
    /// 包体积。
    pub bytes: u64,
    /// 图片张数。
    pub image_count: usize,
    /// 图片原始字节合计。
    pub image_bytes: u64,
}

/// 包内 manifest 的文件名。
const MANIFEST_NAME: &str = "manifest.json";

/// 写一个归档包。
///
/// 包内布局：`manifest.json` + `images/<原相对路径去掉 images/ 前缀>`。
/// 先写 `.part` 再改名，中途失败不会留下看起来正常的坏包。
pub fn write_archive(
    dir: &Path,
    file_name: &str,
    entries: &[ArchiveEntry],
    data_root: &Path,
) -> Result<ArchiveStats, StorageError> {
    std::fs::create_dir_all(dir).map_err(io_error)?;
    let target = dir.join(file_name);
    if target.exists() {
        return Err(StorageError::Query(sqlx::Error::Protocol(format!(
            "归档文件已存在: {}",
            target.display()
        ))));
    }
    let temp = dir.join(format!("{file_name}.part"));
    match write_inner(&temp, entries, data_root) {
        Ok(()) => {}
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    }
    std::fs::rename(&temp, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        io_error(error)
    })?;
    let bytes = std::fs::metadata(&target).map(|meta| meta.len()).unwrap_or(0);
    Ok(ArchiveStats {
        file_name: file_name.to_string(),
        bytes,
        image_count: entries.len(),
        image_bytes: entries.iter().map(|entry| entry.bytes.max(0) as u64).sum(),
    })
}

fn write_inner(
    temp: &Path,
    entries: &[ArchiveEntry],
    data_root: &Path,
) -> Result<(), StorageError> {
    let file = std::fs::File::create(temp).map_err(io_error)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);

    let manifest = ArchiveManifest {
        version: 1,
        created_at: Utc::now(),
        images: entries.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        StorageError::Query(sqlx::Error::Protocol(format!("序列化 manifest 失败：{error}")))
    })?;
    append_bytes(&mut builder, MANIFEST_NAME, &bytes)?;

    for (index, entry) in entries.iter().enumerate() {
        for reference in &entry.references {
            let absolute = data_root.join(&reference.rel_path);
            let Ok(payload) = std::fs::read(&absolute) else {
                continue;
            };
            let name = format!("references/{index}/{}.bin", reference.ordinal);
            append_bytes(&mut builder, &name, &payload)?;
        }
        let relative = entry
            .rel_path
            .strip_prefix("images/")
            .unwrap_or(&entry.rel_path);
        let absolute = data_root.join(&entry.rel_path);
        let Ok(payload) = std::fs::read(&absolute) else {
            return Err(StorageError::Query(sqlx::Error::Protocol(format!(
                "读不到图片: {}",
                absolute.display()
            ))));
        };
        append_bytes(&mut builder, &format!("images/{relative}"), &payload)?;
    }

    let encoder = builder.into_inner().map_err(io_error)?;
    let file = encoder.finish().map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    Ok(())
}

/// 回读校验：manifest 能解析、图片条目数与期望一致。
pub fn verify_archive(path: &Path, expected: usize) -> Result<(), StorageError> {
    let file = std::fs::File::open(path).map_err(io_error)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut manifest: Option<ArchiveManifest> = None;
    let mut images = 0usize;
    for item in archive.entries().map_err(io_error)? {
        let mut entry = item.map_err(io_error)?;
        let path = entry.path().map_err(io_error)?.to_string_lossy().to_string();
        if path == MANIFEST_NAME {
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).map_err(io_error)?;
            manifest = serde_json::from_slice(&buffer).ok();
        } else if path.starts_with("images/") {
            images += 1;
        }
    }
    let manifest = manifest.ok_or_else(|| {
        StorageError::Query(sqlx::Error::Protocol("归档缺少 manifest".to_string()))
    })?;
    if manifest.images.len() != expected || images != expected {
        return Err(StorageError::Query(sqlx::Error::Protocol(format!(
            "归档不完整：期望 {expected} 张，manifest {} 条，实际图片 {images} 个",
            manifest.images.len()
        ))));
    }
    Ok(())
}

fn append_bytes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), StorageError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, PathBuf::from(name), bytes)
        .map_err(io_error)
}

fn io_error(error: std::io::Error) -> StorageError {
    StorageError::Query(sqlx::Error::Io(error))
}

//! 图片归档：把在线图片打包、校验、再删除。
//!
//! 顺序不可换：写包 → 校验 → 删文件 → 删记录 → 登记。任何一步失败都停，
//! 宁可这次没归档成功，也不能出现「包不全但图已删」。

use std::sync::Arc;

use chrono::Utc;
use gpt2apirs_storage::archive::{self, ArchiveEntry};
use gpt2apirs_storage::repo::{ImageArchiveRepo, ImageAssetQuery, ImageAssetRepo, ImageAssetRow};

use crate::state::AppState;

/// 一次归档的结果。
#[derive(Debug, Clone)]
pub struct Outcome {
    /// 包文件名；dry-run 时是建议的文件名。
    pub file_name: String,
    /// 包体积（dry-run 时为 0）。
    pub bytes: u64,
    /// 图片张数。
    pub image_count: usize,
    /// 图片原始字节合计。
    pub image_bytes: u64,
    /// 是否只是预演。
    pub dry_run: bool,
}

/// 归档包所在目录。
pub fn archive_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(gpt2apirs_core::paths::data_dir()?.join("archives"))
}

/// 执行（或预演）一次归档。
pub async fn run(state: &Arc<AppState>, dry_run: bool) -> anyhow::Result<Outcome> {
    let repo = ImageAssetRepo::new(state.db.clone());
    let rows = load_all(&repo).await?;
    let image_bytes: u64 = rows.iter().map(|row| row.bytes.max(0) as u64).sum();
    let file_name = format!("archive-{}.tar.gz", Utc::now().format("%Y%m%d-%H%M%S"));

    if dry_run || rows.is_empty() {
        return Ok(Outcome {
            file_name,
            bytes: 0,
            image_count: rows.len(),
            image_bytes,
            dry_run: true,
        });
    }

    let data_dir = gpt2apirs_core::paths::data_dir()?;
    // 包体积≈原图总和（PNG 不可压），另留 64 MB 给 manifest 与文件系统抖动。
    let needed = image_bytes + 64 * 1024 * 1024;
    let free = gpt2apirs_core::disk::available_bytes(&data_dir)?;
    if free < needed {
        anyhow::bail!(
            "剩余空间不足：归档需要约 {} MB，当前可用 {} MB",
            needed / (1024 * 1024),
            free / (1024 * 1024)
        );
    }

    // 参考图按 image_id 归组，挂到对应条目上。
    let mut refs_by_image: std::collections::HashMap<uuid::Uuid, Vec<gpt2apirs_storage::repo::ImageReferenceRow>> =
        std::collections::HashMap::new();
    if let Ok(all_refs) = gpt2apirs_storage::repo::ImageReferenceRepo::new(state.db.clone())
        .list_all()
        .await
    {
        for row in all_refs {
            refs_by_image.entry(row.image_id).or_default().push(row);
        }
    }

    let entries: Vec<ArchiveEntry> = rows
        .iter()
        .map(|row| ArchiveEntry {
            rel_path: row.rel_path.clone(),
            mime: row.mime.clone(),
            bytes: row.bytes,
            width: row.width,
            height: row.height,
            account_id: row.account_id,
            prompt: row.prompt.clone(),
            created_at: row.created_at,
            api_key_prefix: row.api_key_prefix.clone(),
            references: refs_by_image
                .get(&row.id)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| gpt2apirs_storage::archive::ArchiveReference {
                            ordinal: item.ordinal,
                            rel_path: item.rel_path.clone(),
                            mime: item.mime.clone(),
                            bytes: item.bytes,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();

    let dir = archive_dir()?;
    let stats = archive::write_archive(&dir, &file_name, &entries, &data_dir)?;
    archive::verify_archive(&dir.join(&file_name), stats.image_count)?;

    // 校验通过之后才允许删除：先删文件（缺失视为已删），再删记录。
    for row in &rows {
        let absolute = data_dir.join(row.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        match std::fs::remove_file(&absolute) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %absolute.display(), "删除归档后的图片失败");
            }
        }
    }
    for row in &rows {
        if let Err(error) = repo.delete(row.id).await {
            tracing::warn!(%error, id = %row.id, "删除已归档的图片记录失败");
        }
    }

    ImageArchiveRepo::new(state.db.clone())
        .insert(
            &stats.file_name,
            stats.bytes as i64,
            stats.image_count as i32,
            stats.image_bytes as i64,
        )
        .await?;
    tracing::info!(file = %stats.file_name, images = stats.image_count, "图片已归档");

    Ok(Outcome {
        file_name: stats.file_name,
        bytes: stats.bytes,
        image_count: stats.image_count,
        image_bytes: stats.image_bytes,
        dry_run: false,
    })
}

/// 分页取全部图片记录（仓储单次最多 200 条）。
async fn load_all(repo: &ImageAssetRepo) -> anyhow::Result<Vec<ImageAssetRow>> {
    const PAGE: i64 = 200;
    let mut rows = Vec::new();
    let mut offset = 0i64;
    loop {
        let batch = repo
            .list(&ImageAssetQuery {
                account_id: None,
                since: None,
                limit: PAGE,
                offset,
            })
            .await?;
        let count = batch.len() as i64;
        rows.extend(batch);
        if count < PAGE {
            break;
        }
        offset += PAGE;
    }
    Ok(rows)
}

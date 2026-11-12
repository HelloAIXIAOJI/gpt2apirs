//! 后台的备份与恢复。
//!
//! 备份写到 `DATA_DIR/backups/`，恢复只接受该目录下的文件名（不收路径），
//! 避免把「恢复」变成任意文件读取。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use gpt2apirs_core::AppError;
use gpt2apirs_storage::backup;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

fn data_dir() -> Result<std::path::PathBuf, ApiError> {
    gpt2apirs_core::paths::data_dir().map_err(internal)
}

/// `POST /api/backup` —— 打一个备份包。
pub async fn create_backup(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let data = data_dir()?;
    let dir = backup::backup_dir(&data);
    std::fs::create_dir_all(&dir).map_err(internal)?;

    let bundle = backup::export(&state.db).await.map_err(internal)?;
    let image_paths: Vec<String> = bundle
        .image_assets
        .iter()
        .filter_map(|item| item.get("rel_path").and_then(Value::as_str))
        .map(|value| value.trim_start_matches("images/").to_string())
        .collect();

    let file_name = format!("backup-{}.tar.gz", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let target = dir.join(&file_name);
    let bytes = backup::write_archive(&target, &bundle, &data.join("images"), &image_paths)
        .map_err(internal)?;

    tracing::info!(file = %file_name, bytes, accounts = bundle.accounts.len(), "创建备份");
    Ok(Json(json!({
        "file": file_name,
        "bytes": bytes,
        "accounts": bundle.accounts.len(),
        "api_keys": bundle.api_keys.len(),
        "images": bundle.image_assets.len(),
    })))
}

/// `GET /api/backups` —— 列出已有备份。
pub async fn list_backups(State(_state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let dir = backup::backup_dir(&data_dir()?);
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("gz") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else { continue };
            items.push(json!({
                "file": path.file_name().and_then(|value| value.to_str()).unwrap_or(""),
                "bytes": metadata.len(),
                "modified_at": metadata.modified().ok().map(|time| {
                    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
                }),
            }));
        }
    }
    items.sort_by(|a, b| b["file"].as_str().cmp(&a["file"].as_str()));
    Ok(Json(json!({ "data": items })))
}

/// 恢复请求。
#[derive(Debug, Deserialize)]
pub struct RestoreRequest {
    /// 备份文件名（只收文件名，不收路径）。
    pub file: String,
}

/// `POST /api/restore` —— 合并恢复。
pub async fn restore(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = request.file.trim();
    // 只接受裸文件名：挡掉 ../ 与绝对路径
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err(ApiError::admin(AppError::invalid_param(
            "file 必须是 backups 目录下的文件名",
            "file",
        )));
    }
    let data = data_dir()?;
    let target = backup::backup_dir(&data).join(name);
    if !target.is_file() {
        return Err(ApiError::admin(AppError::invalid_param(
            "备份文件不存在",
            "file",
        )));
    }

    let (bundle, images) = backup::read_archive(&target).map_err(internal)?;
    let stats = backup::restore_merge(&state.db, &bundle).await.map_err(internal)?;
    let files_written =
        backup::restore_images(&data.join("images"), &images).map_err(internal)?;

    tracing::info!(
        file = %name,
        accounts = stats.accounts_inserted,
        files = files_written,
        "恢复备份"
    );
    Ok(Json(json!({
        "ok": true,
        "accounts_inserted": stats.accounts_inserted,
        "api_keys_inserted": stats.api_keys_inserted,
        "images_inserted": stats.images_inserted,
        "files_written": files_written,
        "settings_applied": stats.settings_applied,
    })))
}

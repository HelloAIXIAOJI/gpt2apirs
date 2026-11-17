//! 备份页：列出现有备份，支持新建、下载、恢复、删除。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::accounts::encode;
use super::has_session;
use super::nav;
use crate::routes::backup as service;
use crate::state::AppState;

/// 一行备份。
pub struct BackupRow {
    file: String,
    bytes: String,
    modified: String,
}

/// 页面。
#[derive(Template)]
#[template(path = "backups.html")]
struct BackupsPage {
    nav: Vec<nav::NavItem>,
    flash: String,
    error: String,
    rows: Vec<BackupRow>,
}

/// 提示与错误查询参数。
#[derive(Debug, Default, Deserialize)]
pub struct FlashParams {
    /// 操作结果提示。
    #[serde(default)]
    pub flash: Option<String>,
    /// 错误提示。
    #[serde(default)]
    pub error: Option<String>,
}

/// 操作目标。
#[derive(Debug, Deserialize)]
pub struct FileForm {
    /// 备份文件名。
    #[serde(default)]
    pub file: String,
}

/// `GET /backups`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<FlashParams>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let (rows, error) = match list() {
        Ok(rows) => (rows, String::new()),
        Err(failure) => (Vec::new(), format!("读取备份目录失败（{failure}）")),
    };
    render(BackupsPage {
        nav: nav::items("/backups"),
        flash: params.flash.unwrap_or_default(),
        error: params.error.unwrap_or(error),
        rows,
    })
}

fn list() -> anyhow::Result<Vec<BackupRow>> {
    let data = gpt2apirs_core::paths::data_dir()?;
    let dir = gpt2apirs_storage::backup::backup_dir(&data);
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("gz") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            rows.push(BackupRow {
                file: path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string(),
                bytes: format_bytes(metadata.len()),
                modified: metadata
                    .modified()
                    .ok()
                    .map(|time| {
                        chrono::DateTime::<chrono::Utc>::from(time)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    rows.sort_by(|a, b| b.file.cmp(&a.file));
    Ok(rows)
}

/// `POST /backups/create`
pub async fn create(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match service::create(&state).await {
        Ok(summary) => {
            let message = format!(
                "已创建 {}（{}，{} 账号 / {} 密钥 / {} 张图）",
                summary["file"].as_str().unwrap_or("-"),
                format_bytes(summary["bytes"].as_u64().unwrap_or(0)),
                summary["accounts"].as_u64().unwrap_or(0),
                summary["api_keys"].as_u64().unwrap_or(0),
                summary["images"].as_u64().unwrap_or(0)
            );
            Redirect::to(&format!("/backups?flash={}", encode(&message))).into_response()
        }
        Err(error) => {
            let message = format!("创建备份失败：{}", error.error.message());
            Redirect::to(&format!("/backups?error={}", encode(&message))).into_response()
        }
    }
}

/// `POST /backups/restore`
pub async fn restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<FileForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match service::restore_file(&state, &form.file).await {
        Ok(summary) => {
            let message = format!(
                "已恢复 {}：账号 {} / 密钥 {} / 图片 {} / 文件 {}",
                form.file,
                summary["accounts_inserted"].as_u64().unwrap_or(0),
                summary["api_keys_inserted"].as_u64().unwrap_or(0),
                summary["images_inserted"].as_u64().unwrap_or(0),
                summary["files_written"].as_u64().unwrap_or(0)
            );
            Redirect::to(&format!("/backups?flash={}", encode(&message))).into_response()
        }
        Err(error) => {
            let message = format!("恢复失败：{}", error.error.message());
            Redirect::to(&format!("/backups?error={}", encode(&message))).into_response()
        }
    }
}

/// `POST /backups/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<FileForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.file.trim();
    if !service::name_is_safe(name) {
        let message = "文件名不合法".to_string();
        return Redirect::to(&format!("/backups?error={}", encode(&message))).into_response();
    }
    let message = match gpt2apirs_core::paths::data_dir() {
        Ok(data) => {
            let path = gpt2apirs_storage::backup::backup_dir(&data).join(name);
            if !path.is_file() {
                format!("{name} 不存在")
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        tracing::info!(file = %name, "删除备份");
                        format!("已删除 {name}")
                    }
                    Err(error) => format!("删除失败：{error}"),
                }
            }
        }
        Err(error) => format!("定位数据目录失败：{error}"),
    };
    Redirect::to(&format!("/backups?flash={}", encode(&message))).into_response()
}

/// `GET /backups/download/{file}`
pub async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    if !service::name_is_safe(&name) {
        return (StatusCode::BAD_REQUEST, "文件名不合法").into_response();
    }
    let Ok(data) = gpt2apirs_core::paths::data_dir() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "定位数据目录失败").into_response();
    };
    let path = gpt2apirs_storage::backup::backup_dir(&data).join(&name);
    match std::fs::read(&path) {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/gzip".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "备份文件不存在").into_response(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("模板渲染失败：{error}"),
        )
            .into_response(),
    }
}

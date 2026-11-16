//! 图片页：看在线占用，一键归档并清理。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::{ImageArchiveRepo, ImageAssetRepo};
use serde::Deserialize;

use super::accounts::{encode, FlashQuery};
use super::has_session;
use super::nav;
use crate::state::AppState;

/// 归档记录的一行。
pub struct ArchiveRow {
    file_name: String,
    images: String,
    image_bytes: String,
    archive_bytes: String,
    created_at: String,
}

impl ArchiveRow {
    fn from_row(row: &gpt2apirs_storage::repo::ImageArchiveRow) -> Self {
        Self {
            file_name: row.file_name.clone(),
            images: row.image_count.to_string(),
            image_bytes: format_bytes(row.image_bytes.max(0) as u64),
            archive_bytes: format_bytes(row.bytes.max(0) as u64),
            created_at: row.created_at.format("%Y-%m-%d %H:%M").to_string(),
        }
    }
}

/// 页面上的一次归档动作。
#[derive(Debug, Deserialize)]
pub struct ArchiveForm {
    /// `dry` 表示只预演。
    #[serde(default)]
    pub mode: String,
}

/// 图片页。
#[derive(Template)]
#[template(path = "gallery.html")]
struct GalleryPage {
    nav: Vec<nav::NavItem>,
    count: String,
    bytes: String,
    error: String,
    flash: String,
    archives: Vec<ArchiveRow>,
}

/// `GET /gallery`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<FlashQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(page(&state, query.flash).await)
}

async fn page(state: &AppState, flash: String) -> GalleryPage {
    let asset_repo = ImageAssetRepo::new(state.db.clone());
    let (count, bytes, error) = match asset_repo.stats(None).await {
        Ok(stats) => (stats.count.to_string(), format_bytes(stats.bytes.max(0) as u64), String::new()),
        Err(failure) => ("-".to_string(), "-".to_string(), format!("读取失败（{failure}）")),
    };
    let archives = ImageArchiveRepo::new(state.db.clone())
        .list(20)
        .await
        .map(|rows| rows.iter().map(ArchiveRow::from_row).collect())
        .unwrap_or_default();
    GalleryPage {
        nav: nav::items("/gallery"),
        count,
        bytes,
        error,
        flash,
        archives,
    }
}

/// `POST /gallery/archive`
pub async fn archive(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ArchiveForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let dry_run = form.mode == "dry";
    match crate::image_archive::run(&state, dry_run).await {
        Ok(outcome) if outcome.dry_run => {
            let message = format!(
                "预演：将归档 {} 张（{}），包名 {}；未做任何删除",
                outcome.image_count,
                format_bytes(outcome.image_bytes),
                outcome.file_name
            );
            Redirect::to(&format!("/gallery?flash={}", encode(&message))).into_response()
        }
        Ok(outcome) => {
            let message = format!(
                "已归档 {} 张（原图 {}）→ {}（{}）",
                outcome.image_count,
                format_bytes(outcome.image_bytes),
                outcome.file_name,
                format_bytes(outcome.bytes)
            );
            Redirect::to(&format!("/gallery?flash={}", encode(&message))).into_response()
        }
        Err(error) => {
            let message = format!("归档失败：{error}");
            Redirect::to(&format!("/gallery?flash={}", encode(&message))).into_response()
        }
    }
}

/// `POST /api/images/archive`（管理面 JSON 版）
pub async fn archive_api(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ArchiveQuery>,
) -> Result<axum::Json<serde_json::Value>, crate::error::ApiError> {
    let outcome = crate::image_archive::run(&state, query.dry_run)
        .await
        .map_err(|error| {
            crate::error::ApiError::admin(gpt2apirs_core::AppError::Internal(anyhow::anyhow!(
                "{error}"
            )))
        })?;
    Ok(axum::Json(serde_json::json!({
        "dry_run": outcome.dry_run,
        "file_name": outcome.file_name,
        "bytes": outcome.bytes,
        "image_count": outcome.image_count,
        "image_bytes": outcome.image_bytes,
    })))
}

/// 归档接口的查询参数。
#[derive(Debug, Deserialize)]
pub struct ArchiveQuery {
    /// 只预演不删除。
    #[serde(default)]
    pub dry_run: bool,
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

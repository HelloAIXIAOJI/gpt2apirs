//! 图片页：浏览在线图片（第三方查看器）与归档清理两个标签页。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::{
    ImageArchiveRepo, ImageAssetQuery, ImageAssetRepo, ImageAssetRow, ImageArchiveRow,
};
use serde::Deserialize;

use super::accounts::encode;
use super::has_session;
use super::nav;
use crate::state::AppState;

/// 每页图片数。
const PAGE_SIZE: i64 = 24;

/// 一页里的图片。
pub struct ImageCard {
    url: String,
    thumb: String,
    title: String,
    meta: String,
}

impl ImageCard {
    fn from_row(row: &ImageAssetRow, base: &str) -> Self {
        let title = row.prompt.clone().unwrap_or_else(|| "（无提示词）".to_string());
        Self {
            url: format!("{base}/{}", row.rel_path),
            // 网格用缩略图，灯箱打开原图。
            thumb: format!(
                "{base}/image-thumbnails/{}",
                row.rel_path.trim_start_matches("images/")
            ),
            title: title.chars().take(80).collect(),
            meta: format!(
                "{} · {}",
                row.created_at.format("%Y-%m-%d %H:%M"),
                format_bytes(row.bytes.max(0) as u64)
            ),
        }
    }
}

/// 归档记录的一行。
pub struct ArchiveRow {
    file_name: String,
    images: String,
    image_bytes: String,
    archive_bytes: String,
    created_at: String,
}

impl ArchiveRow {
    fn from_row(row: &ImageArchiveRow) -> Self {
        Self {
            file_name: row.file_name.clone(),
            images: row.image_count.to_string(),
            image_bytes: format_bytes(row.image_bytes.max(0) as u64),
            archive_bytes: format_bytes(row.bytes.max(0) as u64),
            created_at: row.created_at.format("%Y-%m-%d %H:%M").to_string(),
        }
    }
}

/// 标签页与分页参数。
#[derive(Debug, Default, Deserialize)]
pub struct PanelQuery {
    /// `browse` 或 `archive`。
    #[serde(default)]
    pub tab: Option<String>,
    /// 页码，从 1 开始。
    #[serde(default)]
    pub page: Option<i64>,
    /// 提示条。
    #[serde(default)]
    pub flash: Option<String>,
}

/// 页面数据（整页与局部共用）。
struct PanelData {
    tab: String,
    count: String,
    bytes: String,
    error: String,
    images: Vec<ImageCard>,
    page: i64,
    pages: i64,
    prev_href: String,
    next_href: String,
    has_prev: bool,
    has_next: bool,
    archives: Vec<ArchiveRow>,
    tab_browse_href: String,
    tab_archive_href: String,
}

/// 整页。
#[derive(Template)]
#[template(path = "gallery.html")]
struct GalleryPage {
    nav: Vec<nav::NavItem>,
    flash: String,
    data: PanelData,
}

/// 局部面板（htmx 换页用）。
#[derive(Template)]
#[template(path = "gallery_panel.html")]
struct GalleryPanel {
    data: PanelData,
}

/// `GET /gallery`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PanelQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let data = load(&state, &query).await;
    let flash = query.flash.clone().unwrap_or_default();
    render(GalleryPage {
        nav: nav::items("/gallery"),
        flash,
        data,
    })
}

/// `GET /gallery/panel` —— 只回面板内容，供标签与翻页局部刷新。
pub async fn panel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PanelQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let data = load(&state, &query).await;
    render(GalleryPanel { data })
}

async fn load(state: &AppState, query: &PanelQuery) -> PanelData {
    let tab = match query.tab.as_deref() {
        Some("archive") => "archive".to_string(),
        _ => "browse".to_string(),
    };
    let page = query.page.unwrap_or(1).max(1);
    let repo = ImageAssetRepo::new(state.db.clone());
    let mut error = String::new();
    let (count, bytes, total) = match repo.stats(None).await {
        Ok(stats) => (
            stats.count.to_string(),
            format_bytes(stats.bytes.max(0) as u64),
            stats.count,
        ),
        Err(failure) => {
            error = format!("读取失败（{failure}）");
            ("-".to_string(), "-".to_string(), 0)
        }
    };
    let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
    let page = page.min(pages);

    let images = if tab == "browse" {
        match repo
            .list(&ImageAssetQuery {
                account_id: None,
                since: None,
                limit: PAGE_SIZE,
                offset: (page - 1) * PAGE_SIZE,
            })
            .await
        {
            Ok(rows) => rows
                .iter()
                .map(|row| ImageCard::from_row(row, ""))
                .collect(),
            Err(failure) => {
                if error.is_empty() {
                    error = format!("读取图片列表失败（{failure}）");
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let archives = if tab == "archive" {
        ImageArchiveRepo::new(state.db.clone())
            .list(20)
            .await
            .map(|rows| rows.iter().map(ArchiveRow::from_row).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    PanelData {
        tab: tab.clone(),
        count,
        bytes,
        error,
        images,
        page,
        pages,
        prev_href: format!("/gallery?tab={tab}&page={}", (page - 1).max(1)),
        next_href: format!("/gallery?tab={tab}&page={}", (page + 1).min(pages)),
        has_prev: page > 1,
        has_next: page < pages,
        archives,
        tab_browse_href: "/gallery?tab=browse".to_string(),
        tab_archive_href: "/gallery?tab=archive".to_string(),
    }
}

/// 页面上的一次归档动作。
#[derive(Debug, Deserialize)]
pub struct ArchiveForm {
    /// `dry` 表示只预演。
    #[serde(default)]
    pub mode: String,
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
    let message = match crate::image_archive::run(&state, dry_run).await {
        Ok(outcome) if outcome.dry_run => format!(
            "预演：将归档 {} 张（{}），包名 {}；未做任何删除",
            outcome.image_count,
            format_bytes(outcome.image_bytes),
            outcome.file_name
        ),
        Ok(outcome) => format!(
            "已归档 {} 张（原图 {}）→ {}（{}）",
            outcome.image_count,
            format_bytes(outcome.image_bytes),
            outcome.file_name,
            format_bytes(outcome.bytes)
        ),
        Err(error) => format!("归档失败：{error}"),
    };
    Redirect::to(&format!(
        "/gallery?tab=archive&flash={}",
        encode(&message)
    ))
    .into_response()
}

/// `POST /gallery/gc`：清理没有任何记录引用的参考图文件。
pub async fn gc(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let message = match crate::references::collect_garbage(&state).await {
        Ok(removed) => format!("已清理 {removed} 个无引用的参考图文件"),
        Err(error) => format!("清理失败：{error}"),
    };
    Redirect::to(&format!(
        "/gallery?tab=archive&flash={}",
        encode(&message)
    ))
    .into_response()
}

/// `POST /api/images/archive`（管理面 JSON 版）
pub async fn archive_api(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ArchiveQuery>,
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

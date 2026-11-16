//! 后台的图片管理：列表、汇总、删除。
//!
//! 字节在 `DATA_DIR/images`，这里只管索引与文件删除。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use gpt2apirs_core::AppError;
use gpt2apirs_storage::repo::{ImageAssetQuery, ImageAssetRepo};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

/// 列表参数。
#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    /// 条数上限。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 偏移。
    #[serde(default)]
    pub offset: Option<i64>,
    /// 只看该账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 近 N 小时。
    #[serde(default)]
    pub hours: Option<i64>,
}

/// `GET /api/images`
pub async fn list_images(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ImageQuery>,
) -> Result<Json<Value>, ApiError> {
    let repo = ImageAssetRepo::new(state.db.clone());
    let query = ImageAssetQuery {
        account_id: params.account,
        since: params.hours.map(|hours| {
            chrono::Utc::now() - chrono::Duration::hours(hours.clamp(1, 24 * 365))
        }),
        limit: params.limit.unwrap_or(50),
        offset: params.offset.unwrap_or(0),
    };
    let rows = repo.list(&query).await.map_err(internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "rel_path": row.rel_path,
                "url": format!("/{}", row.rel_path),
                "mime": row.mime,
                "bytes": row.bytes,
                "width": row.width,
                "height": row.height,
                "account_id": row.account_id,
                "prompt": row.prompt,
                "created_at": row.created_at,
            })
        })
        .collect();
    Ok(Json(json!({ "data": items, "limit": query.limit, "offset": query.offset })))
}

/// `GET /api/images/stats`
pub async fn image_stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ImageQuery>,
) -> Result<Json<Value>, ApiError> {
    let repo = ImageAssetRepo::new(state.db.clone());
    let since = params.hours.map(|hours| {
        chrono::Utc::now() - chrono::Duration::hours(hours.clamp(1, 24 * 365))
    });
    let stats = repo.stats(since).await.map_err(internal)?;
    Ok(Json(json!({ "count": stats.count, "bytes": stats.bytes })))
}

/// `DELETE /api/images/{id}`
pub async fn delete_image(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let repo = ImageAssetRepo::new(state.db.clone());
    let Some(row) = repo.get(id).await.map_err(internal)? else {
        return Err(ApiError::admin(AppError::invalid_param(
            "图片不存在",
            "id",
        )));
    };
    // 先删文件再删记录：文件删失败时保留记录，便于重试
    let root = gpt2apirs_core::paths::data_dir()
        .map_err(|error| internal(error))?;
    let absolute = root.join(row.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    match std::fs::remove_file(&absolute) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(internal(format!("删除文件失败：{error}")));
        }
    }
    repo.delete(id).await.map_err(internal)?;
    // 清单行随外键级联删除；文件按引用计数决定要不要删。
    crate::references::cleanup_after_image_delete(&state, id).await;
    tracing::info!(path = %row.rel_path, "删除图片");
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/images/references/gc` —— 清理没有任何记录引用的参考图文件。
pub async fn gc_references(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let removed = crate::references::collect_garbage(&state)
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

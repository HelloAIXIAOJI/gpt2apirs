//! 官网「记忆」的查询与删除（管理后台接口）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

/// `GET /api/memories`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<super::conversations::AccountParam>,
) -> Result<Json<Value>, ApiError> {
    let (page, account) = crate::library::memories(&state, params.account).await?;
    let items: Vec<Value> = page
        .entries
        .iter()
        .map(|entry| json!({ "id": entry.id, "content": entry.content }))
        .collect();
    Ok(Json(json!({
        "account_id": account,
        "max_tokens": page.max_tokens,
        "num_tokens": page.num_tokens,
        "used_percent": page.used_percent(),
        "data": items,
    })))
}

/// `GET /api/memories/setting?account=` —— 记忆总开关。
///
/// `enabled` 为 `null` 表示上游没给（未知），不是「关着」。
pub async fn get_setting(
    State(state): State<Arc<AppState>>,
    Query(params): Query<super::conversations::AccountParam>,
) -> Result<Json<Value>, ApiError> {
    let (enabled, account) = crate::library::memory_enabled(&state, params.account).await?;
    Ok(Json(json!({ "account_id": account, "enabled": enabled })))
}

/// `POST /api/memories/setting` —— 改记忆开关（不动已保存的记忆）。
#[derive(Debug, serde::Deserialize)]
pub struct MemorySettingRequest {
    /// 目标账号；不传则按池子选一个。
    #[serde(default)]
    pub account: Option<uuid::Uuid>,
    /// 开或关。
    pub enabled: bool,
}

/// `POST /api/memories/setting` —— 改记忆总开关（已保存的记忆不受影响）。
pub async fn set_setting(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MemorySettingRequest>,
) -> Result<Json<Value>, ApiError> {
    let account =
        crate::library::set_memory_enabled(&state, request.account, request.enabled).await?;
    Ok(Json(json!({ "account_id": account, "enabled": request.enabled })))
}

/// 清空记忆的请求体。
#[derive(Debug, Default, serde::Deserialize)]
pub struct ClearRequest {
    /// 目标账号；不传则按池子选一个。
    #[serde(default)]
    pub account: Option<uuid::Uuid>,
}

/// `POST /api/memories/clear` —— 清空这个账号已保存的记忆。
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ClearRequest>,
) -> Result<Json<Value>, ApiError> {
    let account = crate::library::clear_memories(&state, request.account).await?;
    Ok(Json(json!({ "account_id": account, "cleared": true })))
}

/// `DELETE /api/memories/{id}`
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<super::conversations::AccountParam>,
) -> Result<Json<Value>, ApiError> {
    let account = crate::library::delete_memory(&state, params.account, &id).await?;
    Ok(Json(json!({ "deleted": 1, "id": id, "account_id": account })))
}

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

/// `DELETE /api/memories/{id}`
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<super::conversations::AccountParam>,
) -> Result<Json<Value>, ApiError> {
    let account = crate::library::delete_memory(&state, params.account, &id).await?;
    Ok(Json(json!({ "deleted": 1, "id": id, "account_id": account })))
}

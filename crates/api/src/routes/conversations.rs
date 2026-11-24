//! 官网会话的查询与删除（管理后台接口）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// 查询参数。
#[derive(Debug, Deserialize, Default)]
pub struct ConversationsQuery {
    /// 指定账号；不传就随便用一个可用号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 偏移。
    #[serde(default)]
    pub offset: Option<i64>,
    /// 条数（默认 28，与网页端一致）。
    #[serde(default)]
    pub limit: Option<i64>,
}

/// 删除参数。
#[derive(Debug, Deserialize, Default)]
pub struct AccountParam {
    /// 指定账号。
    #[serde(default)]
    pub account: Option<Uuid>,
}

/// `GET /api/conversations`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ConversationsQuery>,
) -> Result<Json<Value>, ApiError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(28).clamp(1, 100);
    let (page, account) = crate::library::conversations(&state, params.account, offset, limit).await?;
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "title": item.title,
                "created_at": item.created_at,
                "updated_at": item.updated_at,
                "archived": item.archived,
                "starred": item.starred,
                "temporary": item.temporary,
                "no_memory": item.no_memory,
            })
        })
        .collect();
    Ok(Json(json!({
        "account_id": account,
        "offset": page.offset,
        "limit": page.limit,
        "total": page.total,
        "data": items,
    })))
}

/// `DELETE /api/conversations/{id}`
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<Json<Value>, ApiError> {
    let (removed, account) = crate::library::delete_conversation(&state, params.account, &id).await?;
    if !removed {
        // 上游说没这条：重复点删除不该报 500，回 404 让调用方知道「已经不在了」
        return Err(ApiError::admin(gpt2apirs_core::AppError::NotFound {
            message: format!("没有 id 为 {id} 的会话（可能已经删掉了）"),
            code: Some("conversation_not_found".to_string()),
        }));
    }
    Ok(Json(json!({ "deleted": 1, "id": id, "account_id": account })))
}

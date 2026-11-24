//! 后台的日志查询与监控汇总。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use gpt2apirs_core::AppError;
use gpt2apirs_storage::repo::{CallRecordQuery, CallRecordRepo};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// 日志查询参数。
#[derive(Debug, Deserialize)]
pub struct LogQuery {
    /// 条数上限。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 偏移。
    #[serde(default)]
    pub offset: Option<i64>,
    /// 状态段：`2xx` / `4xx` / `5xx` / `all`。
    #[serde(default)]
    pub status: Option<String>,
    /// 路径前缀。
    #[serde(default)]
    pub path: Option<String>,
    /// 账号 ID。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 错误码。
    #[serde(default)]
    pub code: Option<String>,
    /// 近 N 小时。
    #[serde(default)]
    pub hours: Option<i64>,
}

fn status_range(raw: Option<&str>) -> Option<(i32, i32)> {
    match raw.unwrap_or("all").trim().to_ascii_lowercase().as_str() {
        "2xx" => Some((200, 299)),
        "3xx" => Some((300, 399)),
        "4xx" => Some((400, 499)),
        "5xx" => Some((500, 599)),
        _ => None,
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

/// `GET /api/logs`
pub async fn list_logs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let repo = CallRecordRepo::new(state.db.clone());
    let query = CallRecordQuery {
        status_range: status_range(params.status.as_deref()),
        path_prefix: params.path.filter(|value| !value.trim().is_empty()),
        account_id: params.account,
        error_code: params.code.filter(|value| !value.trim().is_empty()),
        since: params
            .hours
            .map(|hours| chrono::Utc::now() - chrono::Duration::hours(hours.clamp(1, 24 * 30))),
        limit: params.limit.unwrap_or(50),
        offset: params.offset.unwrap_or(0),
    };
    let rows = repo.list(&query).await.map_err(internal)?;
    let items: Vec<Value> = rows.iter().map(row_json).collect();
    Ok(Json(json!({
        "data": items,
        "limit": query.limit,
        "offset": query.offset,
    })))
}

fn row_json(row: &gpt2apirs_storage::repo::CallRecordRow) -> Value {
    json!({
        "id": row.id,
        "created_at": row.created_at,
        "method": row.method,
        "path": row.path,
        "status": row.status,
        "duration_ms": row.duration_ms,
        "account_id": row.account_id,
        "request_id": row.request_id,
        "error_code": row.error_code,
        "client_ip": row.client_ip,
        "user_agent": row.user_agent,
        "api_key_prefix": row.api_key_prefix,
        "error_message": row.error_message,
    })
}

/// `GET /api/logs/{id}`
pub async fn get_log(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let repo = CallRecordRepo::new(state.db.clone());
    match repo.get(id).await.map_err(internal)? {
        Some(row) => Ok(Json(row_json(&row))),
        None => Err(ApiError::openai(AppError::NotFound {
            message: format!("没有 id 为 {id} 的调用记录"),
            code: Some("log_not_found".to_string()),
        })),
    }
}

/// `DELETE /api/logs/{id}`
pub async fn delete_log(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let repo = CallRecordRepo::new(state.db.clone());
    let removed = repo.delete(id).await.map_err(internal)?;
    if !removed {
        return Err(ApiError::openai(AppError::NotFound {
            message: format!("没有 id 为 {id} 的调用记录"),
            code: Some("log_not_found".to_string()),
        }));
    }
    Ok(Json(json!({ "deleted": 1, "id": id })))
}

/// `POST /api/logs/delete` —— 按与 `GET /api/logs` 相同的筛选整批删。
pub async fn delete_logs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let repo = CallRecordRepo::new(state.db.clone());
    let query = CallRecordQuery {
        status_range: status_range(params.status.as_deref()),
        path_prefix: params.path.filter(|value| !value.trim().is_empty()),
        account_id: params.account,
        error_code: params.code.filter(|value| !value.trim().is_empty()),
        since: params
            .hours
            .map(|hours| chrono::Utc::now() - chrono::Duration::hours(hours.clamp(1, 24 * 30))),
        limit: 0,
        offset: 0,
    };
    let removed = repo.delete_matching(&query).await.map_err(internal)?;
    Ok(Json(json!({ "deleted": removed })))
}

/// 监控汇总参数。
#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    /// 近 N 小时，默认 24。
    #[serde(default)]
    pub hours: Option<i64>,
}

/// `GET /api/metrics`
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MetricsQuery>,
) -> Result<Json<Value>, ApiError> {
    let hours = params.hours.unwrap_or(24).clamp(1, 24 * 30);
    let repo = CallRecordRepo::new(state.db.clone());
    let summary = repo.summary(hours).await.map_err(internal)?;
    let success_rate = if summary.total > 0 {
        summary.ok as f64 / summary.total as f64
    } else {
        0.0
    };
    Ok(Json(json!({
        "hours": hours,
        "total": summary.total,
        "ok": summary.ok,
        "failed": summary.failed,
        "success_rate": success_rate,
        "p50_ms": summary.p50_ms,
        "p95_ms": summary.p95_ms,
        "top_errors": summary
            .top_errors
            .iter()
            .map(|(code, hits)| json!({ "code": code, "hits": hits }))
            .collect::<Vec<_>>(),
    })))
}


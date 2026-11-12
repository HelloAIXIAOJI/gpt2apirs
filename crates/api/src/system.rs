//! 健康检查与版本端点。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

/// 应用版本（取自 Cargo 包版本）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `GET /version` —— 版本信息。
pub async fn version() -> Json<serde_json::Value> {
    Json(json!({ "version": APP_VERSION }))
}

/// `GET /health` —— 详细信息，**总是 200**。
///
/// 健康与否放在 `status` / `healthy` 字段里，
/// 让监控决定如何告警，而不是直接返回 5xx。
pub async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let database_ok = check_database(&state).await;
    let healthy = database_ok;

    Json(json!({
        "status": if healthy { "ok" } else { "degraded" },
        "healthy": healthy,
        "version": APP_VERSION,
        "uptime_secs": state.uptime_secs(),
        "checks": {
            "database": if database_ok { "ok" } else { "error" },
        },
    }))
}

/// `GET /health/ready` —— 就绪探针，**不健康返回 503**。
///
/// 给编排系统（k8s readiness / systemd watchdog）用。
pub async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if check_database(&state).await {
        (StatusCode::OK, Json(json!({ "status": "ready" }))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not_ready", "reason": "database" })),
        )
            .into_response()
    }
}

async fn check_database(state: &AppState) -> bool {
    match sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(error = %err, "健康检查：数据库不可用");
            false
        }
    }
}

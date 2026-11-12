//! 后台登录与 API key 管理。
//!
//! 登录口令只用来换会话 cookie；调用 `/v1/*` 的 API key 在这里创建与吊销。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use chrono::Utc;
use gpt2apirs_core::AppError;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::is_admin_token;
use crate::error::ApiError;
use crate::session;
use crate::state::AppState;

/// 登录请求。
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// 登录口令（`ADMIN_KEY`）。
    pub key: String,
}

fn is_secure(request_headers: &axum::http::HeaderMap) -> bool {
    request_headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// `POST /admin/login`
pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<(axum::http::HeaderMap, Json<Value>), ApiError> {
    if !is_admin_token(&state.config.admin_key, request.key.trim()) {
        return Err(ApiError::admin(AppError::Unauthorized {
            message: "登录口令不正确".to_string(),
        }));
    }
    let (token, expires_at) = session::issue(state.session_secret.as_ref(), Utc::now());
    let mut response_headers = axum::http::HeaderMap::new();
    let cookie = session::cookie_header(&token, is_secure(&headers));
    response_headers.insert(
        axum::http::header::SET_COOKIE,
        cookie.parse().map_err(|_| {
            ApiError::admin(AppError::Internal(anyhow::anyhow!("cookie 头非法")))
        })?,
    );
    Ok((
        response_headers,
        Json(json!({ "ok": true, "expires_at": expires_at })),
    ))
}

/// `POST /admin/logout`
pub async fn logout() -> (axum::http::HeaderMap, Json<Value>) {
    let mut response_headers = axum::http::HeaderMap::new();
    let _ = response_headers.insert(
        axum::http::header::SET_COOKIE,
        format!("{}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0", session::COOKIE_NAME)
            .parse()
            .expect("清 cookie 头合法"),
    );
    (response_headers, Json(json!({ "ok": true })))
}

/// `GET /admin/session`
pub async fn session_info(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "authenticated": true,
        "api_keys": state.api_keys.active_count().await.unwrap_or(0),
        "session_hours": session::SESSION_HOURS,
    }))
}

/// `GET /api/keys`
pub async fn list_keys(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let rows = state.api_keys.list().await.map_err(internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "name": row.name,
                "prefix": row.prefix,
                "created_at": row.created_at,
                "last_used_at": row.last_used_at,
                "revoked_at": row.revoked_at,
            })
        })
        .collect();
    Ok(Json(json!({ "data": items })))
}

/// 创建 key 的请求。
#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    /// 名称。
    #[serde(default)]
    pub name: String,
}

/// `POST /api/keys`
pub async fn create_key(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateKeyRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = if request.name.trim().is_empty() {
        "未命名"
    } else {
        request.name.trim()
    };
    let created = state.api_keys.create(name).await.map_err(internal)?;
    tracing::info!(name = %created.row.name, prefix = %created.row.prefix, "创建 API key");
    // 明文只在这里返回一次
    Ok(Json(json!({
        "id": created.row.id,
        "name": created.row.name,
        "prefix": created.row.prefix,
        "created_at": created.row.created_at,
        "plaintext": created.plaintext,
    })))
}

/// `DELETE /api/keys/{id}`
pub async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let revoked = state.api_keys.revoke(id).await.map_err(internal)?;
    if !revoked {
        return Err(ApiError::admin(AppError::invalid_param(
            "该 key 不存在或已吊销",
            "id",
        )));
    }
    Ok(Json(json!({ "ok": true })))
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

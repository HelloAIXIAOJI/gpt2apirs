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
                // 列表不带桶的话，改完桶看不出变化（踩过：以为没生效，其实写进去了）
                "bucket_id": row.bucket_id,
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
    /// 绑定的桶；不传 = 全局。
    #[serde(default)]
    pub bucket_id: Option<Uuid>,
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
    let created = state
        .api_keys
        .create(name, request.bucket_id)
        .await
        .map_err(internal)?;
    tracing::info!(name = %created.row.name, prefix = %created.row.prefix, "创建 API key");
    // 明文只在这里返回一次
    Ok(Json(json!({
        "id": created.row.id,
        "name": created.row.name,
        "prefix": created.row.prefix,
        "created_at": created.row.created_at,
        "bucket_id": created.row.bucket_id,
        "plaintext": created.plaintext,
    })))
}

/// 编辑 key 的请求。
#[derive(Debug, Deserialize, Default)]
pub struct UpdateKeyRequest {
    /// 新名称；不传表示不改。
    #[serde(default)]
    pub name: Option<String>,
    /// 绑定的桶。三态：字段不存在 = 不改，`null` = 改成全局，`"<uuid>"` = 改到该桶。
    #[serde(default, deserialize_with = "crate::routes::admin::double_option")]
    pub bucket_id: Option<Option<Uuid>>,
}

/// 把「字段缺失」与「显式 null」分开。
///
/// serde 默认把两者都变成 `None`，那样就没法表达「改成全局」这个动作。
pub fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `PATCH /api/keys/{id}` —— 改名称与所属桶。
pub async fn update_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateKeyRequest>,
) -> Result<Json<Value>, ApiError> {
    if let Some(name) = request.name.as_deref() {
        if name.trim().is_empty() {
            return Err(ApiError::admin(AppError::invalid_param(
                "名称不能为空",
                "name",
            )));
        }
    }
    let updated = state
        .api_keys
        .update(id, request.name.as_deref(), request.bucket_id)
        .await
        .map_err(internal)?;
    if !updated {
        return Err(ApiError::admin(AppError::NotFound {
            message: "key 不存在".to_string(),
            code: Some("key_not_found".to_string()),
        }));
    }
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// `POST /api/keys/{id}/restore` —— 恢复已吊销的 key（明文不变）。
pub async fn restore_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let restored = state.api_keys.restore(id).await.map_err(internal)?;
    if !restored {
        return Err(ApiError::admin(AppError::NotFound {
            message: "key 不存在".to_string(),
            code: Some("key_not_found".to_string()),
        }));
    }
    Ok(Json(json!({ "ok": true, "id": id })))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> UpdateKeyRequest {
        serde_json::from_str(body).expect("应能解析")
    }

    #[test]
    fn bucket_field_distinguishes_absent_null_and_value() {
        // 三态是「改桶」这个功能的全部难点：把 null 和缺失混成一种，
        // 就没法表达「改回全局」，也没法表达「这次不动桶」。
        let absent = parse(r#"{"name":"a"}"#);
        assert!(absent.bucket_id.is_none(), "没传 = 不改桶");

        let null = parse(r#"{"bucket_id":null}"#);
        assert_eq!(null.bucket_id, Some(None), "显式 null = 改成全局");

        let value = parse(r#"{"bucket_id":"00000000-0000-4000-8000-000000000001"}"#);
        assert_eq!(
            value.bucket_id,
            Some(Some(
                uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap()
            )),
            "传了 id = 改到这个桶"
        );
    }
}

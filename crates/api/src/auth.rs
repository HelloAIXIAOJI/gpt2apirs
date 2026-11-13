//! API Key 鉴权。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use gpt2apirs_core::AppError;

use crate::error::{ApiError, ErrorFormat};
use crate::state::AppState;

/// 按请求路径决定错误响应格式。
///
/// 按路径选格式：`/v1/messages` → Anthropic，其余 `/v1*` → OpenAI，其他 → 管理面。
pub fn error_format_for_path(path: &str) -> ErrorFormat {
    if path == "/v1/messages" {
        return ErrorFormat::Anthropic;
    }
    if path == "/v1" || path.starts_with("/v1/") {
        return ErrorFormat::OpenAi;
    }
    ErrorFormat::Admin
}

/// 从 `Authorization` 头里取 Bearer token。
///
/// 大小写不敏感地识别 `Bearer`；缺失或为空返回 `None`。
pub fn extract_bearer_token(value: Option<&str>) -> Option<String> {
    let raw = value.unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    let (scheme, rest) = match raw.split_once(' ') {
        Some(parts) => parts,
        None => return None,
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 常数时间字符串比较。
///
/// 长度不等直接返回 `false`（长度不是秘密）；同长度时逐字节累积差异，
/// 不做提前返回。
pub fn secure_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for index in 0..a.len() {
        diff |= a[index] ^ b[index];
    }
    diff == 0
}

/// 判断给定 token 是否是有效管理员。
pub fn is_admin_token(expected: &str, token: &str) -> bool {
    !expected.is_empty() && !token.is_empty() && secure_eq(expected, token)
}

/// axum 中间件：要求后台身份。
///
/// 两种凭据任一即可：会话 cookie（浏览器），或 `ADMIN_KEY` 的 Bearer（curl）。
/// **不接受** API key —— 那是调用 `/v1/*` 用的。
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let format = error_format_for_path(request.uri().path());
    let now = chrono::Utc::now();

    // 1) 会话 cookie
    let cookie_header = request
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    if let Some(token) = crate::session::cookie_value(cookie_header, crate::session::COOKIE_NAME) {
        if crate::session::verify(state.session_secret.as_ref(), &token, now).is_some() {
            return Ok(next.run(request).await);
        }
    }

    // 2) 登录口令
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match extract_bearer_token(header) {
        None => Err(ApiError {
            error: AppError::Unauthorized {
                message: "需要登录后台（缺少会话 cookie 或 Bearer 口令）".to_string(),
            },
            format,
        }),
        Some(token) => {
            if is_admin_token(&state.config.admin_key, &token) {
                Ok(next.run(request).await)
            } else {
                // 口令不对属于「没通过认证」，不是「已认证但无权限」，所以 401。
                Err(ApiError {
                    error: AppError::Unauthorized {
                        message: "登录口令不正确".to_string(),
                    },
                    format,
                })
            }
        }
    }
}

/// axum 中间件：要求 API key。
///
/// 只认数据库里的 API key；后台登录口令在这里一定失败。
pub async fn require_api_key(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Some(token) = extract_bearer_token(header) else {
        return Err(ApiError::openai(AppError::Unauthorized {
            message: "缺少 API key".to_string(),
        }));
    };
    match state.api_keys.verify(&token).await {
        Ok(Some(_)) => Ok(next.run(request).await),
        Ok(None) => Err(ApiError::openai(AppError::Unauthorized {
            message: "API key 无效或已吊销".to_string(),
        })),
        Err(error) => Err(ApiError::openai(AppError::Internal(anyhow::anyhow!(
            "校验 API key 失败: {error}"
        )))),
    }
}

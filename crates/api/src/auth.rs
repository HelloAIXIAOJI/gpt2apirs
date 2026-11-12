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

/// axum 中间件：要求管理员身份。
///
/// 失败返回 401 / 403（管理面格式）。
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let format = error_format_for_path(request.uri().path());

    let Some(token) = extract_bearer_token(header) else {
        return Err(ApiError {
            error: AppError::Unauthorized {
                message: "缺少 Authorization: Bearer <API Key>".to_string(),
            },
            format,
        });
    };

    if !is_admin_token(&state.config.auth_key, &token) {
        return Err(ApiError {
            error: AppError::Unauthorized {
                message: "密钥无效或已失效，请重新登录".to_string(),
            },
            format,
        });
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction() {
        assert_eq!(
            extract_bearer_token(Some("Bearer abc123")).as_deref(),
            Some("abc123")
        );
        // scheme 大小写不敏感
        assert_eq!(
            extract_bearer_token(Some("bearer abc123")).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_bearer_token(Some("BEARER abc123")).as_deref(),
            Some("abc123")
        );
        // 前后空白
        assert_eq!(
            extract_bearer_token(Some("  Bearer   abc123  ")).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn bearer_extraction_rejects_bad_input() {
        assert_eq!(extract_bearer_token(None), None);
        assert_eq!(extract_bearer_token(Some("")), None);
        assert_eq!(extract_bearer_token(Some("   ")), None);
        assert_eq!(extract_bearer_token(Some("abc123")), None, "缺 scheme");
        assert_eq!(extract_bearer_token(Some("Basic abc")), None, "错 scheme");
        assert_eq!(extract_bearer_token(Some("Bearer ")), None, "空 token");
    }

    #[test]
    fn secure_eq_matches_string_equality() {
        assert!(secure_eq("abc", "abc"));
        assert!(!secure_eq("abc", "abd"));
        assert!(!secure_eq("abc", "ab"));
        assert!(!secure_eq("", "a"));
        assert!(secure_eq("", ""));
    }

    #[test]
    fn error_format_follows_path() {
        assert_eq!(error_format_for_path("/v1/messages"), ErrorFormat::Anthropic);
        assert_eq!(error_format_for_path("/v1"), ErrorFormat::OpenAi);
        assert_eq!(
            error_format_for_path("/v1/images/generations"),
            ErrorFormat::OpenAi
        );
        assert_eq!(error_format_for_path("/api/ping"), ErrorFormat::Admin);
        assert_eq!(error_format_for_path("/health"), ErrorFormat::Admin);
    }

    #[test]
    fn admin_token_check() {
        assert!(is_admin_token("secret-key-123", "secret-key-123"));
        assert!(!is_admin_token("secret-key-123", "wrong"));
        assert!(!is_admin_token("", ""), "空期望值不应通过");
        assert!(!is_admin_token("k", ""), "空 token 不应通过");
    }
}

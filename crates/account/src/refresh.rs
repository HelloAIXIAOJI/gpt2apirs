//! access_token 刷新（OAuth `refresh_token` 授权）。
//!
//!
//! # 请求格式
//!
//! token 端点**只接受 `application/x-www-form-urlencoded`**，发 JSON 会被拒。
//!
//! # 错误分类
//!
//! 刷新失败必须区分「能不能再试」：refresh_token 被判 `invalid_grant` 就是终局，
//! 继续重试只会把上游打爆、把日志刷满。

use std::time::Duration;

use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use serde_json::Value;

use crate::consts::{OAUTH_CLIENT_ID, OAUTH_TOKEN_URL};

/// 刷新成功后的新凭证。
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    /// 新 access token。
    pub access_token: String,
    /// 新 refresh token（上游可能轮换，也可能不返回）。
    pub refresh_token: Option<String>,
    /// 新 id token。
    pub id_token: Option<String>,
}

/// 刷新失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// 终局错误：refresh_token 已失效，重试没有意义。
    #[error("refresh_token 已失效：{0}")]
    Terminal(String),

    /// 可重试错误：网络问题、上游 5xx、限流。
    #[error("刷新暂时失败：{0}")]
    Retryable(String),

    /// 请求本身构造失败或响应不是合法 JSON。
    #[error("刷新请求异常：{0}")]
    Malformed(String),
}

impl RefreshError {
    /// 是否终局。
    pub fn is_terminal(&self) -> bool {
        matches!(self, RefreshError::Terminal(_))
    }
}

/// 刷新请求的超时上限。
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// 用 refresh_token 换一组新的 token。
pub async fn refresh_tokens(
    client: &Client,
    refresh_token: &str,
    deadline: Deadline,
) -> Result<RefreshOutcome, RefreshError> {
    if refresh_token.trim().is_empty() {
        return Err(RefreshError::Terminal("refresh_token 为空".to_string()));
    }
    let timeout = deadline
        .budget("token_refresh", REFRESH_TIMEOUT)
        .unwrap_or(REFRESH_TIMEOUT);

    let headers = vec![
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/x-www-form-urlencoded".to_string()),
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36              (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"
                .to_string(),
        ),
    ];
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.trim().to_string()),
        ("client_id", OAUTH_CLIENT_ID.to_string()),
    ];

    let response = client
        .post_form(OAUTH_TOKEN_URL, &headers, &form, Some(timeout))
        .await
        .map_err(|error| RefreshError::Retryable(error.to_string()))?;

    parse_refresh_response(response.status, &response.body)
}

/// 解析刷新响应。抽成纯函数便于单测。
pub fn parse_refresh_response(status: u16, body: &str) -> Result<RefreshOutcome, RefreshError> {
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);

    if (200..=299).contains(&status) {
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                RefreshError::Malformed("响应里没有 access_token".to_string())
            })?;
        return Ok(RefreshOutcome {
            access_token: access_token.to_string(),
            refresh_token: optional_string(&payload, "refresh_token"),
            id_token: optional_string(&payload, "id_token"),
        });
    }

    let code = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let detail = payload
        .get("error_description")
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .unwrap_or(body)
        .trim()
        .to_string();
    let message = if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    };

    if is_terminal_code(&code) || status == 401 || status == 403 {
        return Err(RefreshError::Terminal(message));
    }
    if status == 429 || status >= 500 {
        return Err(RefreshError::Retryable(message));
    }
    // 400 但没有明确 error code：多半是请求体问题，归为可重试之外的「终局」会误杀，
    // 因此保守地算可重试。
    Err(RefreshError::Retryable(message))
}

/// 这些 `error` 值代表 refresh_token 本身不可用，重试无意义。
fn is_terminal_code(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant" | "invalid_request" | "unauthorized_client" | "invalid_client"
    )
}

fn optional_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_refresh() {
        let body = r#"{"access_token":"at-new","refresh_token":"rt-new","id_token":"id-new"}"#;
        let outcome = parse_refresh_response(200, body).expect("应成功");
        assert_eq!(outcome.access_token, "at-new");
        assert_eq!(outcome.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(outcome.id_token.as_deref(), Some("id-new"));
    }

    #[test]
    fn keeps_existing_refresh_token_when_absent() {
        let body = r#"{"access_token":"at-new"}"#;
        let outcome = parse_refresh_response(200, body).expect("应成功");
        assert!(outcome.refresh_token.is_none(), "上游未轮换时不伪造");
    }

    #[test]
    fn rejects_success_without_access_token() {
        let error = parse_refresh_response(200, r#"{"scope":"x"}"#).unwrap_err();
        assert!(matches!(error, RefreshError::Malformed(_)), "{error}");
    }

    #[test]
    fn invalid_grant_is_terminal() {
        let body = r#"{"error":"invalid_grant","error_description":"token expired"}"#;
        let error = parse_refresh_response(400, body).unwrap_err();
        assert!(error.is_terminal(), "{error}");
        assert!(error.to_string().contains("token expired"), "{error}");
    }

    #[test]
    fn unauthorized_status_is_terminal() {
        let error = parse_refresh_response(401, "{}").unwrap_err();
        assert!(error.is_terminal());
    }

    #[test]
    fn server_error_is_retryable() {
        let error = parse_refresh_response(503, "upstream down").unwrap_err();
        assert!(!error.is_terminal(), "{error}");
        assert!(matches!(error, RefreshError::Retryable(_)));
    }

    #[test]
    fn rate_limit_is_retryable() {
        let error = parse_refresh_response(429, "{}").unwrap_err();
        assert!(!error.is_terminal());
    }

    #[test]
    fn unknown_400_is_retryable_not_terminal() {
        let error = parse_refresh_response(400, "{}").unwrap_err();
        assert!(!error.is_terminal(), "无明确 error code 时不应误杀账号");
    }

    #[test]
    fn error_without_body_keeps_status() {
        let error = parse_refresh_response(500, "").unwrap_err();
        assert!(error.to_string().contains("500"), "{error}");
    }
}

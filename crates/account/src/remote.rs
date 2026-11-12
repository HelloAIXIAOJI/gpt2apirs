//! 上游账号信息读取（`/me` + `conversation/init` + `accounts/check`）。
//!
//! 图片配额不在 `/me` 里，而在 `conversation/init` 返回的 `limits_progress` 中，
//! 套餐类型在 `accounts/check` 里。三个接口都必须调。

use std::time::Duration;

use chrono::{DateTime, Utc};
use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::endpoints::{
    ACCOUNTS_CHECK, BASE_URL, CONVERSATION_INIT, ME,
};
use serde_json::Value;

use crate::model::Plan;

/// 一次远端读取的结果。
#[derive(Debug, Clone, Default)]
pub struct RemoteInfo {
    /// 邮箱。
    pub email: Option<String>,
    /// 上游 account_id。
    pub upstream_account_id: Option<String>,
    /// 套餐。
    pub plan: Option<Plan>,
    /// 剩余图片配额。
    pub quota: i32,
    /// 限流恢复时间。
    pub restore_at: Option<DateTime<Utc>>,
    /// 账号默认模型。
    pub default_model_slug: Option<String>,
    /// 原始 `limits_progress`（留档排查用）。
    pub limits_progress: Vec<Value>,
}

/// 远端读取错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum RemoteError {
    /// token 无效（上游 401/403）。出现这个说明账号真的废了。
    #[error("access token 无效：{0}")]
    Unauthorized(String),

    /// 上游返回非 2xx。
    #[error("{path} 返回 {status}: {body}")]
    Status {
        /// 路径。
        path: String,
        /// 状态码。
        status: u16,
        /// 响应体（已截断）。
        body: String,
    },

    /// 网络/解析错误。
    #[error("{0}")]
    Transport(String),
}

impl RemoteError {
    /// 是否属于「账号已失效」。
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, RemoteError::Unauthorized(_))
    }
}

/// 每个子请求的超时上限。
const PER_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 读取一个账号的远端信息。
pub async fn fetch_remote_info(
    client: &Client,
    access_token: &str,
    user_agent: &str,
    deadline: Deadline,
) -> Result<RemoteInfo, RemoteError> {
    let budget = |stage: &'static str| -> Duration {
        deadline
            .budget(stage, PER_CALL_TIMEOUT)
            .unwrap_or(PER_CALL_TIMEOUT)
    };

    // 三个请求互不依赖，并发发起；任一失败即整体失败。
    let me = get_json(client, ME, user_agent, access_token, budget("remote_me"));
    let init = post_json(
        client,
        CONVERSATION_INIT,
        user_agent,
        access_token,
        serde_json::json!({
            "gizmo_id": null,
            "requested_default_model": null,
            "conversation_id": null,
            "timezone_offset_min": -480,
        }),
        budget("remote_init"),
    );
    let accounts_path = format!("{ACCOUNTS_CHECK}?timezone_offset_min=-480");
    let accounts = get_json(
        client,
        &accounts_path,
        user_agent,
        access_token,
        budget("remote_accounts"),
    );

    let (me, init, accounts) = tokio::try_join!(me, init, accounts)?;

    let mut info = RemoteInfo {
        email: me
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        upstream_account_id: me
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        ..Default::default()
    };

    let default_account = accounts
        .get("accounts")
        .and_then(|a| a.get("default"))
        .and_then(|d| d.get("account"));
    if let Some(plan_type) = default_account.and_then(|a| a.get("plan_type")).and_then(Value::as_str) {
        info.plan = Some(Plan::parse(plan_type));
    }
    if info.upstream_account_id.is_none() {
        info.upstream_account_id = default_account
            .and_then(|a| a.get("account_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
    }

    if let Some(progress) = init.get("limits_progress").and_then(Value::as_array) {
        info.limits_progress = progress.clone();
        if let Some(item) = progress
            .iter()
            .find(|item| item.get("feature_name").and_then(Value::as_str) == Some("image_gen"))
        {
            info.quota = item
                .get("remaining")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, i32::MAX as i64) as i32;
            info.restore_at = item
                .get("reset_after")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .and_then(parse_timestamp);
        }
    }
    // 没有 image_gen 条目时配额未知，保持 0 会误判为限流；
    // 用一个显式标记让调用方知道「本次没读到配额」。
    info.default_model_slug = init
        .get("default_model_slug")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());

    Ok(info)
}

/// 解析上游的时间字符串。
///
/// 上游可能给 RFC3339，也可能给纯数字时间戳。两种都接住，都不行就返回 `None`。
fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(seconds) = text.parse::<i64>() {
        return DateTime::from_timestamp(seconds, 0);
    }
    None
}

async fn get_json(
    client: &Client,
    path: &str,
    user_agent: &str,
    token: &str,
    timeout: Duration,
) -> Result<Value, RemoteError> {
    let headers = gpt2apirs_http::api_headers(user_agent, token, path);
    let response = client
        .get_text(&format!("{BASE_URL}{path}"), &headers, Some(timeout))
        .await
        .map_err(|error| RemoteError::Transport(error.to_string()))?;
    interpret(path, response.status, &response.body)
}

async fn post_json(
    client: &Client,
    path: &str,
    user_agent: &str,
    token: &str,
    body: Value,
    timeout: Duration,
) -> Result<Value, RemoteError> {
    let headers = gpt2apirs_http::api_headers(user_agent, token, path);
    let response = client
        .post_json(&format!("{BASE_URL}{path}"), &headers, &body, Some(timeout))
        .await
        .map_err(|error| RemoteError::Transport(error.to_string()))?;
    interpret(path, response.status, &response.body)
}

fn interpret(path: &str, status: u16, body: &str) -> Result<Value, RemoteError> {
    match status {
        200..=299 => serde_json::from_str(body)
            .map_err(|error| RemoteError::Transport(format!("{path} 响应不是合法 JSON: {error}"))),
        401 | 403 => Err(RemoteError::Unauthorized(truncate(body))),
        other => Err(RemoteError::Status {
            path: path.to_string(),
            status: other,
            body: truncate(body),
        }),
    }
}

fn truncate(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let clipped: String = text.chars().take(LIMIT).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_401_to_unauthorized() {
        let error = interpret("/backend-api/me", 401, "{}").unwrap_err();
        assert!(error.is_unauthorized());
    }

    #[test]
    fn maps_403_to_unauthorized() {
        assert!(interpret("/x", 403, "").unwrap_err().is_unauthorized());
    }

    #[test]
    fn maps_500_to_status_error() {
        let error = interpret("/x", 500, "boom").unwrap_err();
        assert!(!error.is_unauthorized());
        match error {
            RemoteError::Status { status, .. } => assert_eq!(status, 500),
            other => panic!("期望 Status，实际 {other:?}"),
        }
    }

    #[test]
    fn parses_json_on_2xx() {
        let value = interpret("/x", 200, r#"{"email":"a@b.com"}"#).unwrap();
        assert_eq!(value["email"], "a@b.com");
    }

    #[test]
    fn rejects_invalid_json_on_2xx() {
        assert!(matches!(
            interpret("/x", 200, "not json"),
            Err(RemoteError::Transport(_))
        ));
    }

    #[test]
    fn truncates_long_bodies() {
        let long = "x".repeat(1000);
        let error = interpret("/x", 500, &long).unwrap_err();
        match error {
            RemoteError::Status { body, .. } => {
                assert!(body.chars().count() <= 302, "应截断: {}", body.chars().count());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_both_timestamp_forms() {
        assert!(parse_timestamp("2026-11-13T16:32:22Z").is_some());
        assert!(parse_timestamp("1763047942").is_some());
        assert!(parse_timestamp("not a time").is_none());
    }
}

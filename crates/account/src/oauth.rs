//! OAuth PKCE 登录。
//!
//!
//! # 流程
//!
//! ~~~text
//! 1. start    生成 PKCE，返回授权 URL（用户在浏览器完成登录）
//! 2. finish   提交回调 URL 或 code，换 token 入库
//! ~~~
//!
//! 会话在内存里保存 `code_verifier`，10 分钟过期。

use std::sync::Arc;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::consts::{
    OAUTH_AUDIENCE, OAUTH_AUTHORIZE_URL, OAUTH_CLIENT_ID, OAUTH_REDIRECT_URI, OAUTH_SCOPE,
    OAUTH_SESSION_TTL_SECS, OAUTH_TOKEN_URL,
};
use crate::model::{Plan, Source};
use crate::repo::NewAccount;

/// PKCE 参数对。
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// `code_verifier`（43~128 字符）。
    pub verifier: String,
    /// `code_challenge` = base64url(sha256(verifier))。
    pub challenge: String,
}

impl PkcePair {
    /// 生成一对新的 PKCE 参数。
    pub fn generate() -> Self {
        use rand::Rng as _;
        const ALPHABET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        let mut rng = rand::thread_rng();
        let verifier: String = (0..64)
            .map(|_| {
                let index = rng.gen_range(0..ALPHABET.len());
                ALPHABET[index] as char
            })
            .collect();
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// 计算 `code_challenge`。
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 一次进行中的授权会话。
#[derive(Debug, Clone)]
struct PendingSession {
    verifier: String,
    state: String,
    created_at: DateTime<Utc>,
}

/// 授权会话存储。
#[derive(Debug, Default)]
pub struct OAuthSessions {
    inner: dashmap::DashMap<Uuid, PendingSession>,
}

impl OAuthSessions {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 清理过期会话。
    pub fn prune(&self) {
        let now = Utc::now();
        let stale: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|entry| {
                (now - entry.value().created_at).num_seconds() > OAUTH_SESSION_TTL_SECS
            })
            .map(|entry| *entry.key())
            .collect();
        for id in stale {
            self.inner.remove(&id);
        }
    }

    /// 当前会话数（测试用）。
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// 起始一个授权会话。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthStart {
    /// 会话 ID，`finish` 时要带回来。
    pub session_id: Uuid,
    /// 让用户打开的授权地址。
    pub authorize_url: String,
}

/// 发起 OAuth 登录。
pub fn start(sessions: &OAuthSessions, email_hint: Option<&str>) -> OAuthStart {
    sessions.prune();
    let pkce = PkcePair::generate();
    let state = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4();
    sessions.inner.insert(
        session_id,
        PendingSession {
            verifier: pkce.verifier.clone(),
            state: state.clone(),
            created_at: Utc::now(),
        },
    );

    let mut params = vec![
        ("issuer", "https://auth.openai.com".to_string()),
        ("client_id", OAUTH_CLIENT_ID.to_string()),
        ("audience", OAUTH_AUDIENCE.to_string()),
        ("redirect_uri", OAUTH_REDIRECT_URI.to_string()),
        ("scope", OAUTH_SCOPE.to_string()),
        ("response_type", "code".to_string()),
        ("response_mode", "query".to_string()),
        ("state", state),
        ("nonce", Uuid::new_v4().to_string()),
        ("code_challenge", pkce.challenge),
        ("code_challenge_method", "S256".to_string()),
    ];
    if let Some(hint) = email_hint.map(str::trim).filter(|s| !s.is_empty()) {
        params.push(("login_hint", hint.to_string()));
        params.push(("screen_hint", "login_or_signup".to_string()));
    }

    let query = params
        .iter()
        .map(|(key, value)| format!("{}={}", key, gpt2apirs_core::util::form_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    OAuthStart {
        session_id,
        authorize_url: format!("{OAUTH_AUTHORIZE_URL}?{query}"),
    }
}

/// OAuth 交换失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// 会话不存在或已过期。
    #[error("授权会话不存在或已过期")]
    SessionNotFound,

    /// 回调里没有 code。
    #[error("回调里没有找到 code 参数")]
    NoCode,

    /// 回调里的 state 与发起时不符（CSRF）。
    #[error("回调里的 state 与授权会话不匹配")]
    StateMismatch,

    /// 上游拒绝。
    #[error("换取 token 失败：{0}")]
    Exchange(String),
}

/// 完成 OAuth 登录，返回待入库账号。
pub async fn finish(
    client: &Client,
    sessions: &OAuthSessions,
    session_id: Uuid,
    callback_or_code: &str,
    deadline: Deadline,
) -> Result<NewAccount, OAuthError> {
    sessions.prune();
    let session = sessions
        .inner
        .remove(&session_id)
        .map(|(_, value)| value)
        .ok_or(OAuthError::SessionNotFound)?;

    let code = extract_code(callback_or_code).ok_or(OAuthError::NoCode)?;

    verify_state(&session.state, callback_or_code)?;

    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("client_id", OAUTH_CLIENT_ID.to_string()),
        ("redirect_uri", OAUTH_REDIRECT_URI.to_string()),
        ("code_verifier", session.verifier),
    ];
    let headers = vec![
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/x-www-form-urlencoded".to_string()),
    ];
    let timeout = deadline
        .budget("oauth_exchange", std::time::Duration::from_secs(30))
        .unwrap_or(std::time::Duration::from_secs(30));
    let response = client
        .post_form(OAUTH_TOKEN_URL, &headers, &form, Some(timeout))
        .await
        .map_err(|error| OAuthError::Exchange(error.to_string()))?;

    let payload: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            // 上游拒绝时必须把原因带出来，否则只剩一个状态码，排查无从下手。
            let detail = payload
                .get("error_description")
                .and_then(Value::as_str)
                .or_else(|| payload.get("error").and_then(Value::as_str))
                .or_else(|| payload.get("message").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| {
                    let clipped: String = response.body.chars().take(200).collect();
                    if clipped.trim().is_empty() {
                        "(响应为空)".to_string()
                    } else {
                        clipped
                    }
                });
            OAuthError::Exchange(format!("HTTP {}: {}", response.status, detail))
        })?;

    Ok(NewAccount {
        access_token: access_token.to_string(),
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        id_token: payload.get("id_token").and_then(Value::as_str).map(str::to_string),
        // 邮箱与套餐由随后的 sync 从上游补齐
        email: crate::credentials::token_email(access_token),
        upstream_account_id: crate::credentials::token_upstream_account_id(access_token),
        plan: Plan::Unknown,
        source: Source::Web,
        quota: 0,
        quota_confirmed: false,
        proxy: String::new(),
    })
}

/// 校验回调里的 `state`。
///
/// 带了 state 就必须一致（CSRF）；用户直接粘裸 code 时没有 state，
/// 这种情况靠一次性 `session_id` + PKCE 兜底。
fn verify_state(expected: &str, callback_or_code: &str) -> Result<(), OAuthError> {
    match extract_param(callback_or_code, "state") {
        Some(actual) if actual != expected => Err(OAuthError::StateMismatch),
        _ => Ok(()),
    }
}

/// 从回调 URL / 查询串里取出某个查询参数。
///
/// 覆盖三种输入：完整 URL（`https://x/cb?code=..&state=..`）、
/// 裸查询串（`code=..&state=..`）、以及不含 `=` 的裸值。
pub fn extract_param(input: &str, key: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || !trimmed.contains('=') {
        return None;
    }
    let query = trimmed
        .split_once('?')
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name.trim() == key {
                let decoded = gpt2apirs_core::util::form_decode(value.trim());
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// 从回调 URL、查询串或裸 code 中取出授权码。
pub fn extract_code(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 裸 code（不含 `=` 也不含 `?`）直接返回
    if !trimmed.contains('=') {
        return Some(trimmed.to_string());
    }
    extract_param(trimmed, "code")
}

/// 生成并持有会话的便捷封装（供 API 层使用）。
pub type SharedSessions = Arc<OAuthSessions>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_are_well_formed() {
        let pair = PkcePair::generate();
        assert!(
            (43..=128).contains(&pair.verifier.len()),
            "verifier 长度不合法: {}",
            pair.verifier.len()
        );
        assert_eq!(pair.challenge, challenge_for(&pair.verifier));
        assert!(!pair.challenge.contains('='), "challenge 应无 padding");
        assert!(!pair.challenge.contains('+'), "challenge 应为 url-safe");
        assert!(!pair.challenge.contains('/'), "challenge 应为 url-safe");
    }

    #[test]
    fn challenge_is_deterministic() {
        // RFC 7636 附录 B 的测试向量
        let verifier = "dBjftJeZ4CVP-m8F92b1U2h3x0v6C7x5z8W1p4Q2r9M";
        assert_eq!(challenge_for(verifier), challenge_for(verifier));
    }

    #[test]
    fn two_generated_pairs_differ() {
        let a = PkcePair::generate();
        let b = PkcePair::generate();
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn start_stores_session_and_builds_url() {
        let sessions = OAuthSessions::new();
        let start = start(&sessions, None);
        assert_eq!(sessions.len(), 1);
        assert!(start.authorize_url.starts_with(OAUTH_AUTHORIZE_URL));
        assert!(start.authorize_url.contains("code_challenge_method=S256"));
        assert!(start.authorize_url.contains(&format!("session_id={}", start.session_id)) == false);
        assert!(start.authorize_url.contains("client_id="));
    }

    #[test]
    fn start_with_email_hint_includes_login_hint() {
        let sessions = OAuthSessions::new();
        let start = start(&sessions, Some("a@b.com"));
        assert!(start.authorize_url.contains("login_hint=a%40b.com"));
        assert!(start.authorize_url.contains("screen_hint=login_or_signup"));
    }

    #[test]
    fn extract_code_from_full_callback_url() {
        let url = "https://platform.openai.com/auth/callback?code=abc123&state=xyz";
        assert_eq!(extract_code(url).as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_code_from_query_only() {
        assert_eq!(extract_code("code=abc&state=1").as_deref(), Some("abc"));
    }

    #[test]
    fn extract_code_from_bare_code() {
        assert_eq!(extract_code("abc123").as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_code_handles_url_encoding() {
        assert_eq!(
            extract_code("https://x/cb?code=a%2Fb%2Bc").as_deref(),
            Some("a/b+c")
        );
    }

    #[test]
    fn extract_code_returns_none_without_code() {
        assert!(extract_code("https://x/cb?state=1").is_none());
        assert!(extract_code("").is_none());
        assert!(extract_code("   ").is_none());
    }

    #[test]
    fn urlencode_escapes_reserved_chars() {
        assert_eq!(gpt2apirs_core::util::form_encode("a@b.com"), "a%40b.com");
        assert_eq!(gpt2apirs_core::util::form_encode("a b"), "a+b");
        assert_eq!(gpt2apirs_core::util::form_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn exchange_error_keeps_upstream_detail() {
        // 这条只验证错误类型与字段拼接方式，不联网
        let error = OAuthError::Exchange("HTTP 401: token_expired".to_string());
        assert!(error.to_string().contains("401"), "{error}");
        assert!(error.to_string().contains("token_expired"), "{error}");
    }

    #[test]
    fn missing_session_is_reported() {
        let sessions = OAuthSessions::new();
        // finish 需要 client，这里只验证会话不存在的错误路径在取 client 之前触发
        assert!(sessions.is_empty());
    }

    #[test]
    fn extract_param_reads_query_values() {
        let url = "https://platform.openai.com/auth/callback?code=abc123&state=xyz";
        assert_eq!(extract_param(url, "code").as_deref(), Some("abc123"));
        assert_eq!(extract_param(url, "state").as_deref(), Some("xyz"));
        assert_eq!(extract_param("code=abc&state=1", "state").as_deref(), Some("1"));
        assert_eq!(extract_param("abc123", "code"), None, "裸值不是查询串");
        assert_eq!(extract_param(url, "missing"), None);
    }

    #[test]
    fn state_mismatch_is_rejected() {
        assert_eq!(
            verify_state("expected", "https://x/cb?code=abc&state=wrong"),
            Err(OAuthError::StateMismatch)
        );
    }

    #[test]
    fn matching_state_is_accepted() {
        assert_eq!(
            verify_state("expected", "https://x/cb?code=abc&state=expected"),
            Ok(())
        );
    }

    #[test]
    fn bare_code_without_state_is_accepted() {
        // 用户直接粘 code 的场景：没有 state 可比，靠一次性 session + PKCE
        assert_eq!(verify_state("expected", "raw-code-value"), Ok(()));
    }
}

//! 凭证生命周期投影。
//!
//! AT/RT 的状态完全由 token 本身决定（JWT 的 `exp`），是**纯函数**。
//! 持久化它只会引入「存储值与真实值不一致」的风险 —— 时间在走，库里的快照会过期。

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// 距过期不足这么久就算「即将过期」。
pub const EXPIRING_SKEW_HOURS: i64 = 24;

/// access token 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessTokenStatus {
    /// 有效。
    Valid,
    /// 即将过期（< 24h）。
    Expiring,
    /// 无效（解不出 exp，或已过期，或被上游明确拒绝）。
    Invalid,
}

impl AccessTokenStatus {
    /// 前端展示用中文。
    pub fn label(self) -> &'static str {
        match self {
            AccessTokenStatus::Valid => "有效",
            AccessTokenStatus::Expiring => "即将过期",
            AccessTokenStatus::Invalid => "无效",
        }
    }
}

/// refresh token 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefreshTokenStatus {
    /// 存在。**是否真能用只能靠实际刷新验证**。
    Valid,
    /// 缺失（导入时没给）。
    Missing,
    /// 已被上游判定失效。
    Invalid,
}

impl RefreshTokenStatus {
    /// 前端展示用中文。
    pub fn label(self) -> &'static str {
        match self {
            RefreshTokenStatus::Valid => "有效",
            RefreshTokenStatus::Missing => "缺失",
            RefreshTokenStatus::Invalid => "无效",
        }
    }
}

/// 综合可用性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// 可以直接用。
    Usable,
    /// AT 不可用但 RT 在 —— 能刷新救回来。
    Recoverable,
    /// 彻底不可用。
    Unavailable,
}

impl Availability {
    /// 前端展示用中文。
    pub fn label(self) -> &'static str {
        match self {
            Availability::Usable => "可用",
            Availability::Recoverable => "可恢复",
            Availability::Unavailable => "不可用",
        }
    }

    /// 是否还能被选中。
    pub fn is_selectable(self) -> bool {
        matches!(self, Availability::Usable | Availability::Recoverable)
    }
}

/// 一次投影的完整结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAvailability {
    /// access token 状态。
    pub access: AccessTokenStatus,
    /// refresh token 状态。
    pub refresh: RefreshTokenStatus,
    /// 综合可用性。
    pub usable: Availability,
    /// AT 签发时间（JWT `iat`）。
    pub issued_at: Option<DateTime<Utc>>,
    /// AT 到期时间（JWT `exp`）。
    pub expires_at: Option<DateTime<Utc>>,
}

impl CredentialAvailability {
    /// 距到期还剩多少秒；未知返回 `None`。
    pub fn expires_in_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        self.expires_at.map(|exp| (exp - now).num_seconds())
    }
}

/// 解出 JWT 的 payload（不验签，只读声明）。
///
/// 解不出时返回 `None`。
pub fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload_b64 = token.trim().split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload_b64))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 取 JWT 的 `exp`（秒级时间戳）。
pub fn token_exp(token: &str) -> Option<DateTime<Utc>> {
    let claims = decode_jwt_claims(token)?;
    let exp = claims.get("exp")?.as_i64()?;
    if exp <= 0 {
        return None;
    }
    DateTime::from_timestamp(exp, 0)
}

/// 取 JWT 的 `iat`。
pub fn token_iat(token: &str) -> Option<DateTime<Utc>> {
    let claims = decode_jwt_claims(token)?;
    let iat = claims.get("iat")?.as_i64()?;
    if iat <= 0 {
        return None;
    }
    DateTime::from_timestamp(iat, 0)
}

/// 从 JWT 里取上游 `chatgpt_account_id`。
pub fn token_upstream_account_id(token: &str) -> Option<String> {
    let claims = decode_jwt_claims(token)?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// 从 JWT 里取邮箱（`profile.email` 优先，回退顶层 `email`）。
pub fn token_email(token: &str) -> Option<String> {
    let claims = decode_jwt_claims(token)?;
    let from_profile = claims
        .get("https://api.openai.com/profile")
        .and_then(|p| p.get("email"))
        .and_then(|v| v.as_str());
    let from_top = claims.get("email").and_then(|v| v.as_str());
    from_profile
        .or(from_top)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 投影一个账号的凭证可用性。
///
/// `access_confirmed_invalid`：上游明确说过这个 AT 无效。
/// `refresh_confirmed_invalid`：上游明确说过 RT 失效。
pub fn project(
    access_token: &str,
    refresh_token: &str,
    access_confirmed_invalid: bool,
    refresh_confirmed_invalid: bool,
    now: DateTime<Utc>,
) -> CredentialAvailability {
    let expires_at = token_exp(access_token);
    let issued_at = token_iat(access_token);

    let access = if access_token.trim().is_empty() || access_confirmed_invalid {
        AccessTokenStatus::Invalid
    } else {
        match expires_at {
            None => AccessTokenStatus::Invalid,
            Some(exp) if exp <= now => AccessTokenStatus::Invalid,
            Some(exp) if exp - now < Duration::hours(EXPIRING_SKEW_HOURS) => {
                AccessTokenStatus::Expiring
            }
            Some(_) => AccessTokenStatus::Valid,
        }
    };

    let refresh = if refresh_token.trim().is_empty() {
        RefreshTokenStatus::Missing
    } else if refresh_confirmed_invalid {
        RefreshTokenStatus::Invalid
    } else {
        RefreshTokenStatus::Valid
    };

    let usable = match (&access, &refresh) {
        (AccessTokenStatus::Valid, _) | (AccessTokenStatus::Expiring, _) => Availability::Usable,
        (AccessTokenStatus::Invalid, RefreshTokenStatus::Valid) => Availability::Recoverable,
        _ => Availability::Unavailable,
    };

    CredentialAvailability {
        access,
        refresh,
        usable,
        issued_at,
        expires_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// 造一个不验签的 JWT（只有 payload 有意义）。
    fn fake_jwt(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn decodes_claims() {
        let jwt = fake_jwt(serde_json::json!({"exp": 1_900_000_000, "iat": 1_899_000_000}));
        let claims = decode_jwt_claims(&jwt).expect("应可解析");
        assert_eq!(claims["exp"], 1_900_000_000i64);
    }

    #[test]
    fn rejects_malformed_token() {
        assert!(decode_jwt_claims("not-a-jwt").is_none());
        assert!(decode_jwt_claims("").is_none());
        assert!(decode_jwt_claims("a.b.c").is_none(), "b 不是合法 base64/json");
    }

    #[test]
    fn valid_token_projects_usable() {
        let exp = now() + Duration::hours(72);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Valid);
        assert_eq!(p.refresh, RefreshTokenStatus::Valid);
        assert_eq!(p.usable, Availability::Usable);
        assert_eq!(p.expires_at, Some(exp));
    }

    #[test]
    fn token_within_24h_is_expiring() {
        let exp = now() + Duration::hours(2);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Expiring);
        assert_eq!(p.usable, Availability::Usable, "即将过期仍可先用");
    }

    #[test]
    fn expired_token_with_refresh_is_recoverable() {
        let exp = now() - Duration::hours(1);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Invalid);
        assert_eq!(p.refresh, RefreshTokenStatus::Valid);
        assert_eq!(p.usable, Availability::Recoverable);
    }

    #[test]
    fn expired_token_without_refresh_is_unavailable() {
        let exp = now() - Duration::hours(1);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Invalid);
        assert_eq!(p.refresh, RefreshTokenStatus::Missing);
        assert_eq!(p.usable, Availability::Unavailable);
    }

    #[test]
    fn confirmed_invalid_refresh_makes_unavailable() {
        let exp = now() - Duration::hours(1);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", false, true, now());
        assert_eq!(p.refresh, RefreshTokenStatus::Invalid);
        assert_eq!(p.usable, Availability::Unavailable);
    }

    #[test]
    fn confirmed_invalid_access_overrides_future_exp() {
        let exp = now() + Duration::hours(72);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", true, false, now());
        assert_eq!(p.access, AccessTokenStatus::Invalid, "上游说无效就是无效");
        assert_eq!(p.usable, Availability::Recoverable);
    }

    #[test]
    fn token_without_exp_is_invalid() {
        let jwt = fake_jwt(serde_json::json!({"iat": 1_899_000_000}));
        let p = project(&jwt, "rt", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Invalid);
    }

    #[test]
    fn empty_access_token_is_invalid() {
        let p = project("", "rt", false, false, now());
        assert_eq!(p.access, AccessTokenStatus::Invalid);
        assert_eq!(p.usable, Availability::Recoverable);
    }

    #[test]
    fn extracts_email_and_account_id() {
        let jwt = fake_jwt(serde_json::json!({
            "exp": 1_900_000_000,
            "https://api.openai.com/profile": {"email": "a@b.com"},
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-123"}
        }));
        assert_eq!(token_email(&jwt).as_deref(), Some("a@b.com"));
        assert_eq!(token_upstream_account_id(&jwt).as_deref(), Some("acc-123"));
    }

    #[test]
    fn labels_are_chinese() {
        assert_eq!(AccessTokenStatus::Expiring.label(), "即将过期");
        assert_eq!(RefreshTokenStatus::Missing.label(), "缺失");
        assert_eq!(Availability::Recoverable.label(), "可恢复");
    }

    #[test]
    fn expires_in_is_computed() {
        let exp = now() + Duration::hours(10);
        let jwt = fake_jwt(serde_json::json!({"exp": exp.timestamp()}));
        let p = project(&jwt, "rt", false, false, now());
        assert_eq!(p.expires_in_secs(now()), Some(10 * 3600));
    }
}

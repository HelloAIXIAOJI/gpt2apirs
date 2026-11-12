//! 后台会话 cookie 的签发与校验。
//!
//! 无状态：cookie 里放「过期时间 + HMAC」，服务端不存会话。
//! 签名密钥由 `ADMIN_KEY` 派生，所以改登录口令会让所有会话立即失效。

use chrono::{DateTime, Duration, Timelike, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// cookie 名。
pub const COOKIE_NAME: &str = "g2a_admin";

/// 会话有效期（小时）。
pub const SESSION_HOURS: i64 = 12;

/// 从 `ADMIN_KEY` 派生签名密钥。
pub fn derive_secret(admin_key: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gpt2apirs-admin-session|");
    hasher.update(admin_key.as_bytes());
    hasher.finalize().into()
}

/// 签发：返回 `v1.<过期时间戳>.<hex 签名>`。
pub fn issue(secret: &[u8], now: DateTime<Utc>) -> (String, DateTime<Utc>) {
    // token 里只存整数秒，签发时就把纳秒截掉，否则签发值与校验值对不上。
    let expires_at = (now + Duration::hours(SESSION_HOURS))
        .with_nanosecond(0)
        .unwrap_or_else(|| now + Duration::hours(SESSION_HOURS));
    let payload = format!("v1.{}", expires_at.timestamp());
    let signature = sign(secret, &payload);
    (format!("{payload}.{signature}"), expires_at)
}

/// 校验：通过返回过期时间，否则 `None`。
pub fn verify(secret: &[u8], token: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let (payload, signature) = token.rsplit_once('.')?;
    let expected = sign(secret, payload);
    if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        return None;
    }
    let stamp = payload.strip_prefix("v1.")?;
    let seconds: i64 = stamp.parse().ok()?;
    let expires_at = DateTime::from_timestamp(seconds, 0)?;
    if expires_at <= now {
        return None;
    }
    Some(expires_at)
}

fn sign(secret: &[u8], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for index in 0..a.len() {
        diff |= a[index] ^ b[index];
    }
    diff == 0
}

/// 从 `Cookie` 头里取指定名字的值。
pub fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    let raw = header?;
    raw.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key.trim() == name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// 拼 `Set-Cookie` 值。`secure` 为真时加 `Secure`。
pub fn cookie_header(token: &str, secure: bool) -> String {
    let mut value = format!(
        "{COOKIE_NAME}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        SESSION_HOURS * 3600
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_token_verifies() {
        let secret = derive_secret("admin-key");
        let now = Utc::now();
        let (token, expires_at) = issue(&secret, now);
        let parsed = verify(&secret, &token, now).expect("应通过");
        assert_eq!(parsed, expires_at);
    }

    #[test]
    fn expired_token_is_rejected() {
        let secret = derive_secret("admin-key");
        let now = Utc::now();
        let (token, _) = issue(&secret, now);
        let later = now + Duration::hours(SESSION_HOURS + 1);
        assert!(verify(&secret, &token, later).is_none());
    }

    #[test]
    fn tampered_token_is_rejected() {
        let secret = derive_secret("admin-key");
        let (token, _) = issue(&secret, Utc::now());
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged_expiry = format!("v1.{}", Utc::now().timestamp() + 99999);
        parts[0] = "v1";
        let forged = format!("{forged_expiry}.{}", parts[2]);
        assert!(verify(&secret, &forged, Utc::now()).is_none(), "改过期时间必须失效");
        assert!(verify(&secret, &token, Utc::now()).is_some());
    }

    #[test]
    fn different_admin_key_invalidates_sessions() {
        let now = Utc::now();
        let (token, _) = issue(&derive_secret("key-a"), now);
        assert!(verify(&derive_secret("key-b"), &token, now).is_none());
    }

    #[test]
    fn cookie_is_httponly_and_scoped() {
        let header = cookie_header("abc", false);
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("SameSite=Lax"));
        assert!(!header.contains("Secure"));
        assert!(cookie_header("abc", true).contains("; Secure"));
    }

    #[test]
    fn cookie_value_picks_named_pair() {
        let header = "a=1; g2a_admin=xyz; b=2";
        assert_eq!(cookie_value(Some(header), COOKIE_NAME).as_deref(), Some("xyz"));
        assert!(cookie_value(Some("a=1"), COOKIE_NAME).is_none());
        assert!(cookie_value(None, COOKIE_NAME).is_none());
    }
}

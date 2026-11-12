//! 通用小工具。
//!
//! 与图片/协议强相关的工具函数不放这里，等对应模块再移植。

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// 生成 uuid4 字符串。
pub fn new_uuid() -> String {
    Uuid::new_v4().to_string()
}

/// 当前 UTC 时间。
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// 当前 Unix 秒（带小数），用于 SSE payload 的 `create_time`。
pub fn now_unix_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_v4_shape() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_ne!(new_uuid(), new_uuid(), "两次生成不应相同");
    }

    #[test]
    fn now_unix_secs_is_plausible() {
        let now = now_unix_secs_f64();
        // 2024-01-01 之后、2100 年之前
        assert!(now > 1_704_067_200.0, "时间戳过小: {now}");
        assert!(now < 4_102_444_800.0, "时间戳过大: {now}");
    }
}

/// `application/x-www-form-urlencoded` 编码。
///
/// 按 RFC 3986 unreserved 集合保留，空格编码为 `+`。
/// 放在 core 是因为 http（OAuth 表单请求）与 account（PKCE 授权 URL）都要用，
/// 两处各写一份必然出现不一致。
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `application/x-www-form-urlencoded` 解码（只处理 `%XX` 与 `+`）。
pub fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 把键值对编码成表单请求体。
pub fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod form_tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(form_encode("a@b.com"), "a%40b.com");
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(form_encode("a/b+c=d"), "a%2Fb%2Bc%3Dd");
    }

    #[test]
    fn decodes_round_trip() {
        for raw in ["a@b.com", "a b", "a/b+c=d", "中文"] {
            assert_eq!(form_decode(&form_encode(raw)), raw, "round trip 失败: {raw}");
        }
    }

    #[test]
    fn decodes_literal_plus_as_space() {
        assert_eq!(form_decode("a+b"), "a b");
    }

    #[test]
    fn malformed_percent_is_preserved() {
        assert_eq!(form_decode("100%"), "100%");
        assert_eq!(form_decode("%zz"), "%zz");
    }

    #[test]
    fn builds_form_body() {
        assert_eq!(
            form_body(&[("grant_type", "refresh_token"), ("a", "b c")]),
            "grant_type=refresh_token&a=b+c"
        );
    }
}

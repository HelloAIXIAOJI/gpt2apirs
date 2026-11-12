//! 日志脱敏：递归地把敏感值掩码。
//!
//! 日志封装容易被当成「只负责输出」，实际核心是一个**递归脱敏器** ——
//! 任何深度、任何结构里的 token / base64 / 密钥都要被处理，
//! 否则一条 WARN 就能把凭据打进日志。

use std::sync::OnceLock;

use base64::Engine as _;
use once_cell::sync::Lazy;
use regex::Regex;

/// 保留前 N 个字符，其余替换成 `...`。
///
/// 掩码字符串：保留前 `keep` 个字符，其余用占位符代替。
pub fn mask_secret(value: &str, keep: usize) -> String {
    if value.chars().count() <= keep {
        return value.to_string();
    }
    let head: String = value.chars().take(keep).collect();
    format!("{head}...")
}

/// 把 base64 串换成长度标记。
///
/// 掩码 base64：只报长度，输出形如 `<base64 len=1880696>`。
pub fn mask_base64(value: &str, keep: usize) -> String {
    // 支持 data URL 前缀
    if let Some((header, data)) = value.split_once(',') {
        if value.starts_with("data:") {
            let head = mask_secret(data, keep);
            return format!("{header},{head} (base64 len={})", data.len());
        }
    }
    format!("{} (base64 len={})", mask_secret(value, keep), value.len())
}

/// 判断一个字符串是否「看起来像 base64」。
///
/// 判断是否像 base64 串：
/// 长度 ≥ 64、长度是 4 的倍数、含 `+/` 或 `=` 中至少一个、且能成功解码。
pub fn is_base64_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 64 || bytes.len() % 4 != 0 {
        return false;
    }
    if !value.contains('+') && !value.contains('/') && !value.contains('=') {
        return false;
    }
    base64::engine::general_purpose::STANDARD.decode(bytes).is_ok()
}

static DATA_URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"data:image/[^;]+;base64,[A-Za-z0-9+/=]+").expect("data URL 正则")
});

static B64_JSON_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)("b64_json"s*:s*")([A-Za-z0-9+/=]+)(")"#).expect("b64_json 正则")
});

/// 键名含 token / 等于 dx 的赋值。覆盖 `token="xxx"`、`access_token=xxx`、`dx: "xxx"` 等。
static TOKEN_ASSIGN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)("?[A-Za-z0-9_-]*token[A-Za-z0-9_-]*"?|"?dx"?)(s*[:=]s*)"?([A-Za-z0-9._~/+-]{20,})"?"#,
    )
    .expect("token 赋值正则")
});

/// 一段可能很长的 base64。后面会逐个校验，不是无脑替换。
static LONG_B64_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/]{64,}={0,2}").expect("长 base64 正则"));

fn global_config() -> &'static (usize, usize) {
    static CFG: OnceLock<(usize, usize)> = OnceLock::new();
    // (secret_keep, base64_keep) —— 与 Python 默认值一致
    CFG.get_or_init(|| (10, 24))
}

/// 对一行日志文本做脱敏。
///
/// 幂等：重复调用结果不变（脱敏后的结果无法再被匹配）。
pub fn redact_line(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let (secret_keep, b64_keep) = *global_config();

    // 1) 内嵌 data URL
    let mut out = DATA_URL_RE
        .replace_all(text, |caps: &regex::Captures| {
            mask_base64(caps.get(0).map(|m| m.as_str()).unwrap_or(""), b64_keep)
        })
        .into_owned();

    // 2) ""b64_json":"....""
    out = B64_JSON_RE
        .replace_all(&out, |caps: &regex::Captures| {
            let head = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let data = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let tail = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            format!("{head}{}{tail}", mask_base64(data, b64_keep))
        })
        .into_owned();

    // 3) 键名含 token 或等于 dx 的长值
    out = TOKEN_ASSIGN_RE
        .replace_all(&out, |caps: &regex::Captures| {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let sep = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let value = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            // 已经脱敏过的（含 "..."）跳过
            if value.ends_with("...") {
                return caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string();
            }
            format!("{key}{sep}\"{}\"", mask_secret(value, secret_keep))
        })
        .into_owned();

    // 4) 任意长 base64 段（逐个校验后再替换）
    out = LONG_B64_RE
        .replace_all(&out, |caps: &regex::Captures| {
            let candidate = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            if is_base64_like(candidate) {
                mask_base64(candidate, b64_keep)
            } else {
                candidate.to_string()
            }
        })
        .into_owned();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_keeps_head() {
        assert_eq!(mask_secret("abcdefghijklmno", 10), "abcdefghij...");
        assert_eq!(mask_secret("short", 10), "short");
        assert_eq!(mask_secret("", 10), "");
    }

    #[test]
    fn mask_base64_reports_length() {
        let s = "A".repeat(100);
        assert_eq!(mask_base64(&s, 24), format!("{}... (base64 len=100)", "A".repeat(24)));
    }

    #[test]
    fn mask_base64_handles_data_url() {
        let s = format!("data:image/png;base64,{}", "B".repeat(80));
        let masked = mask_base64(&s, 24);
        assert!(masked.starts_with("data:image/png;base64,"));
        assert!(masked.contains("base64 len=80"));
    }

    #[test]
    fn is_base64_like_requires_signals() {
        // 太短
        assert!(!is_base64_like(&"A".repeat(63)));
        // 长度对但既不含 + / = 也解码失败
        assert!(!is_base64_like(&"A".repeat(64)));
        // 纯字母数字、无 + / = 信号：即使可解码也不算（与 Python 一致）
        assert!(!is_base64_like(&"QUJD".repeat(16)));
        // 含 = 补位、可解码、长度 4 的倍数  算
        assert!(is_base64_like("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="));
    }

    #[test]
    fn redact_masks_json_b64() {
        // 注意：掩码保留前 24 字符（与 Python _mask_string(keep=24) 一致），
        // 所以样本必须长于 24 字符，否则按语义「原样返回」。
        let payload = format!("iVBORw0KGgo{}", "A".repeat(200));
        let line = format!(r#"{{"b64_json":"{payload}"}}"#);
        let out = redact_line(&line);
        assert!(!out.contains(&"A".repeat(200)), "base64 未被脱敏: {out}");
        assert!(out.contains("base64 len="), "应带长度标记: {out}");
    }

    #[test]
    fn redact_masks_token_assignment() {
        let line = "access_token=eyJhbGciOiJSUzI1NiIsImtpZCI6IjEifQ.payloadhere";
        let out = redact_line(line);
        assert!(!out.contains("payloadhere"), "token 未被脱敏: {out}");
        assert!(out.contains("access_token="));
    }

    #[test]
    fn redact_masks_embedded_data_url() {
        let line = format!("body=prefix data:image/png;base64,{} suffix", "C".repeat(120));
        let out = redact_line(&line);
        assert!(!out.contains(&"C".repeat(120)), "内嵌 data URL 未被脱敏");
        assert!(out.contains("prefix"));
        assert!(out.contains("suffix"));
    }

    #[test]
    fn redact_is_idempotent() {
        let line = r#"{"b64_json":"iVBORw0KGgoAAAANSUhEUg=="}"#;
        let once = redact_line(line);
        let twice = redact_line(&once);
        assert_eq!(once, twice, "脱敏必须幂等");
    }

    #[test]
    fn redact_leaves_normal_text_alone() {
        let line = "GET /backend-api/f/conversation -> 200 in 8123ms";
        assert_eq!(redact_line(line), line);
    }
}

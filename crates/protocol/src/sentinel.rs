//! Sentinel 前置校验：请求体构造与响应解析（**纯逻辑，无 HTTP**）。
//!
//!
//! # 流程
//!
//! ```text
//! POST prepare  { "p": <legacy requirements token> }
//!      ↓  { prepare_token, proofofwork:{seed,difficulty,required}, arkose, turnstile }
//! 求解 PoW（若 required）
//! POST finalize { prepare_token, proof_token, turnstile_token }
//!      ↓  { token, so_token, expire_after }
//! ```

use serde_json::{Value, json};

/// prepare 的请求体。
pub fn build_prepare_payload(p_token: &str) -> Value {
    json!({ "p": p_token })
}

/// finalize 的请求体。
pub fn build_finalize_payload(
    prepare_token: &str,
    proof_token: &str,
    turnstile_token: &str,
) -> Value {
    json!({
        "prepare_token": prepare_token,
        "proof_token": proof_token,
        "turnstile_token": turnstile_token,
    })
}

/// PoW 挑战。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowChallenge {
    /// 种子。
    pub seed: String,
    /// 难度（十六进制）。
    pub difficulty: String,
}

impl PowChallenge {
    /// 是否可用于求解（两者都非空）。
    pub fn is_usable(&self) -> bool {
        !self.seed.is_empty() && !self.difficulty.is_empty()
    }
}

/// prepare 响应。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SentinelPrepare {
    /// 下一步要回传的 prepare token。
    pub prepare_token: String,
    /// PoW 挑战（`required=true` 时非空）。
    pub pow: Option<PowChallenge>,
    /// 是否要求 Arkose（本项目不支持）。
    pub arkose_required: bool,
    /// 是否要求 Turnstile。
    pub turnstile_required: bool,
    /// Turnstile 字节码（`dx`，base64）。求解 token 的输入之一。
    pub turnstile_dx: Option<String>,
}

/// 解析 prepare 响应。
pub fn parse_prepare(body: &Value) -> SentinelPrepare {
    let prepare_token = body
        .get("prepare_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let pow = body
        .get("proofofwork")
        .filter(|value| {
            value
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|value| PowChallenge {
            seed: value
                .get("seed")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            difficulty: value
                .get("difficulty")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });

    let arkose_required = body
        .get("arkose")
        .and_then(|value| value.get("required"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let turnstile = body.get("turnstile");
    let turnstile_required = turnstile
        .and_then(|value| value.get("required"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let turnstile_dx = turnstile
        .and_then(|value| value.get("dx"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    SentinelPrepare {
        prepare_token,
        pow,
        arkose_required,
        turnstile_required,
        turnstile_dx,
    }
}

/// finalize 响应。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SentinelFinalize {
    /// 后续请求要带的 requirements token。
    pub token: String,
    /// so_token（当前不使用）。
    pub so_token: String,
    /// 有效期（秒）。
    pub expire_after: Option<i64>,
}

/// 解析 finalize 响应。
pub fn parse_finalize(body: &Value) -> SentinelFinalize {
    SentinelFinalize {
        token: body
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        so_token: body
            .get("so_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expire_after: body.get("expire_after").and_then(Value::as_i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_payload_shape() {
        let payload = build_prepare_payload("gAAAAACxxx");
        assert_eq!(payload["p"], "gAAAAACxxx");
    }

    #[test]
    fn finalize_payload_shape() {
        let payload = build_finalize_payload("pt", "proof", "");
        assert_eq!(payload["prepare_token"], "pt");
        assert_eq!(payload["proof_token"], "proof");
        assert_eq!(payload["turnstile_token"], "");
    }

    #[test]
    fn parse_prepare_with_pow() {
        let body = json!({
            "prepare_token": "pt-123",
            "proofofwork": { "required": true, "seed": "0.0513", "difficulty": "06e75e" },
            "arkose": { "required": false },
            "turnstile": { "required": true, "dx": "abcdefghij" }
        });
        let parsed = parse_prepare(&body);
        assert_eq!(parsed.prepare_token, "pt-123");
        assert!(parsed.pow.is_some());
        let pow = parsed.pow.unwrap();
        assert_eq!(pow.seed, "0.0513");
        assert_eq!(pow.difficulty, "06e75e");
        assert!(pow.is_usable());
        assert!(!parsed.arkose_required);
        assert!(parsed.turnstile_required);
        assert_eq!(parsed.turnstile_dx.as_deref(), Some("abcdefghij"));
    }

    #[test]
    fn parse_prepare_without_pow() {
        let body = json!({ "prepare_token": "pt", "proofofwork": { "required": false } });
        let parsed = parse_prepare(&body);
        assert!(parsed.pow.is_none(), "required=false 时不应有挑战");
        assert!(!parsed.turnstile_required);
    }

    #[test]
    fn parse_prepare_handles_missing_fields() {
        let parsed = parse_prepare(&json!({}));
        assert_eq!(parsed, SentinelPrepare::default());
    }

    #[test]
    fn parse_finalize_extracts_token() {
        let body = json!({
            "token": "req-token",
            "so_token": "so-1",
            "expire_after": 540,
            "persona": "chatgpt-freeaccount"
        });
        let parsed = parse_finalize(&body);
        assert_eq!(parsed.token, "req-token");
        assert_eq!(parsed.so_token, "so-1");
        assert_eq!(parsed.expire_after, Some(540));
    }

    #[test]
    fn parse_finalize_handles_missing_token() {
        let parsed = parse_finalize(&json!({}));
        assert!(parsed.token.is_empty());
        assert_eq!(parsed.expire_after, None);
    }
}

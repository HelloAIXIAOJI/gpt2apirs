//! ChatGPT Sentinel PoW 求解。
//!

//! # 算法

use std::fmt::Write as _;

use base64::Engine as _;
use serde_json::Value;
use sha3::{Digest, Sha3_512};

/// 求解器默认迭代上限（上游实测值）。
pub const DEFAULT_LIMIT: u64 = 500_000;

/// 未求解时的兜底前缀（上游实测值）。
const FALLBACK_PREFIX: &str = "wQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D";

/// PoW 求解结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowAnswer {
    /// base64 编码后的 JSON（已满足难度要求），或兜底字符串。
    pub answer: String,
    /// 是否成功求解。`false` 表示返回的是兜底值。
    pub solved: bool,
    /// 命中的迭代下标，仅 `solved == true` 时有意义。
    pub nonce: Option<u64>,
}

/// 把单个浮点数格式化成 Python `repr()` 的样子。
///
/// 规则（CPython `format_float_short`，`'r'` 模式）：
///
/// 1. 取最短往返十进制表示，得到有效数字与小数点位置 decpt。
/// 2. 若 -4 <= decpt <= 17，用定点表示；否则用科学计数法。
/// 3. 科学计数法的指数带符号且至少两位（e-07 / e+17）。
/// 4. 定点表示的整数值补 .0。
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }

    // Rust 的 {:e} 给出最短往返的科学计数法，例如 "1.23456789e-7"。
    let sci = format!("{value:e}");
    let (mantissa, exp) = match sci.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (sci.as_str(), 0),
    };

    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let int_len = mantissa.split('.').next().map_or(1, |s| s.len()) as i32;
    let decpt = exp + int_len;
    let len = digits.len() as i32;

    let mut out = String::new();
    if negative {
        out.push('-');
    }

    // CPython: decpt <= -4 或 decpt > 16 时用科学计数法
    if (-3..=16).contains(&decpt) {
        // 定点表示
        if decpt <= 0 {
            out.push_str("0.");
            for _ in 0..(-decpt) {
                out.push('0');
            }
            out.push_str(&digits);
        } else if decpt >= len {
            out.push_str(&digits);
            for _ in 0..(decpt - len) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            let (head, tail) = digits.split_at(decpt as usize);
            let _ = write!(out, "{head}.{tail}");
        }
    } else {
        // 科学计数法
        let first = &digits[..1];
        out.push_str(first);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let exp_value = decpt - 1;
        let sign = if exp_value < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", exp_value.abs());
    }

    out
}
/// 把 JSON 值序列化成 Python `json.dumps(separators=(",", ":"), ensure_ascii=False)` 的形式。
pub fn python_json_compact(value: &Value) -> String {
    let mut out = String::new();
    write_python_json(value, &mut out);
    out
}

fn write_python_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                let _ = write!(out, "{int}");
            } else if let Some(uint) = number.as_u64() {
                let _ = write!(out, "{uint}");
            } else if let Some(float) = number.as_f64() {
                out.push_str(&python_float_repr(float));
            } else {
                out.push_str("null");
            }
        }
        Value::String(text) => {
            // serde_json 的字符串转义与 Python ensure_ascii=False 一致
            match serde_json::to_string(text) {
                Ok(encoded) => out.push_str(&encoded),
                Err(_) => out.push_str("\"\""),
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_python_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                match serde_json::to_string(key) {
                    Ok(encoded) => out.push_str(&encoded),
                    Err(_) => out.push('"'),
                }
                out.push(':');
                write_python_json(item, out);
            }
            out.push('}');
        }
    }
}

/// 把一串 JSON 值序列化后按逗号连接（对应 Python 的 join）。
fn join_compact(items: &[Value]) -> String {
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_python_json(item, &mut out);
    }
    out
}

/// 按上游格式切分配置，构造三段静态前缀/后缀。
///
/// 上游把 config 拆成 `[:3]`、`[4:9]`、`[10:]` 三段，段间以逗号相连；
/// 拼出来的字符序列与紧凑 JSON 完全一致（不重新序列化）。
pub fn build_static_parts(config: &[Value]) -> anyhow::Result<(String, String, String)> {
    if config.len() < 10 {
        anyhow::bail!("config 至少需要 10 个元素，实际 {}", config.len());
    }
    let static_1 = format!("[{},", join_compact(&config[..3]));
    let static_2 = format!(",{},", join_compact(&config[4..9]));
    let static_3 = format!(",{}]", join_compact(&config[10..]));
    Ok((static_1, static_2, static_3))
}

/// 求解 PoW。
///
/// 枚举 `limit` 次哈希，直到结果满足 `difficulty`。
pub fn pow_generate(
    seed: &str,
    difficulty: &str,
    config: &[Value],
    limit: u64,
) -> anyhow::Result<PowAnswer> {
    let target = hex_to_bytes(difficulty)?;
    if target.is_empty() {
        anyhow::bail!("difficulty 不能为空");
    }
    let diff_len = difficulty.len() / 2;
    let (static_1, static_2, static_3) = build_static_parts(config)?;
    let seed_bytes = seed.as_bytes();

    let mut final_json =
        String::with_capacity(static_1.len() + static_2.len() + static_3.len() + 16);

    for i in 0..limit {
        final_json.clear();
        final_json.push_str(&static_1);
        let _ = write!(final_json, "{i}");
        final_json.push_str(&static_2);
        let _ = write!(final_json, "{}", i >> 1);
        final_json.push_str(&static_3);

        let encoded =
            base64::engine::general_purpose::STANDARD.encode(final_json.as_bytes());

        let mut hasher = Sha3_512::new();
        hasher.update(seed_bytes);
        hasher.update(encoded.as_bytes());
        let digest = hasher.finalize();

        if digest[..diff_len] <= target[..] {
            return Ok(PowAnswer {
                answer: encoded,
                solved: true,
                nonce: Some(i),
            });
        }
    }

    let fallback_json = format!("\"{seed}\"");
    let fallback = format!(
        "{FALLBACK_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(fallback_json.as_bytes())
    );
    Ok(PowAnswer {
        answer: fallback,
        solved: false,
        nonce: None,
    })
}

/// 把十六进制字符串转成字节。对应 Python `bytes.fromhex`。
fn hex_to_bytes(hex: &str) -> anyhow::Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("difficulty 长度必须为偶数：{hex}");
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("非法十六进制字符：{}", byte as char),
    }
}


// 对外入口：证明 token 与 legacy requirements token 的构造

/// proof token 前缀（上游实测值）。
pub const PROOF_TOKEN_PREFIX: &str = "gAAAAAB";

/// legacy requirements token 前缀（上游实测值）。
pub const LEGACY_REQUIREMENTS_PREFIX: &str = "gAAAAAC";

/// 构造 proof token。
///
/// 求解失败时返回错误（调用方决定降级还是失败）。
pub fn build_proof_token(
    seed: &str,
    difficulty: &str,
    config: &[Value],
) -> anyhow::Result<String> {
    let answer = pow_generate(seed, difficulty, config, DEFAULT_LIMIT)?;
    if !answer.solved {
        anyhow::bail!("failed to solve proof token: difficulty={difficulty}");
    }
    Ok(format!("{PROOF_TOKEN_PREFIX}{}", answer.answer))
}

/// 构造 legacy requirements token（不含 PoW 求解），
/// 构造 legacy requirements token（`gAAAAAC` 前缀）。
pub fn build_legacy_requirements_token(config: &[Value]) -> String {
    let json = python_json_compact(&Value::Array(config.to_vec()));
    let encoded = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    format!("{LEGACY_REQUIREMENTS_PREFIX}{encoded}")
}

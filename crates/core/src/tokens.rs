//! 图片 token 估算。
//!
//! # 必须原样保留的一个上游怪癖
//!
//! 这看起来是 bug，但它是**既定契约**：usage 数字必须与上游口径一致，
//! 否则同一张图两边报的 token 数不同。因此这里**原样保留**，并把签名里的 `model`
//! 标注为 `_model` 以显式表达「接收但不使用」。

use std::io::Cursor;

use base64::Engine as _;
use serde_json::{Value, json};

/// 默认图片尺寸。
pub const DEFAULT_IMAGE_SIZE: (u32, u32) = (1024, 1024);

/// 输入图片 token 的估算模型（硬编码，见模块文档）。
pub const IMAGE_INPUT_TOKEN_MODEL: &str = "gpt-5.4-mini";

/// patch 边长。
pub const PATCH_SIZE: f64 = 32.0;
/// tile 边长。
pub const TILE_SIZE: f64 = 512.0;
/// tile 计算时短边放大到该值。
pub const TILE_HIGH_SHORT_SIDE: f64 = 768.0;

/// 使用 1536 patch 预算的模型前缀。
pub const PATCH_1536_MODELS: &[&str] = &[
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5-codex-mini",
    "gpt-5.1-codex-mini",
    "gpt-5.2-codex",
    "gpt-5.2-chat-latest",
    "o4-mini",
    "gpt-4.1-mini",
    "gpt-4.1-nano",
];

/// patch 数  token 的乘数（按模型前缀）。
pub const PATCH_MULTIPLIERS: &[(&str, f64)] = &[
    ("gpt-5.4-mini", 1.62),
    ("gpt-5.4-nano", 2.46),
    ("gpt-5-mini", 1.62),
    ("gpt-5-nano", 2.46),
    ("gpt-4.1-mini", 1.62),
    ("gpt-4.1-nano", 2.46),
    ("o4-mini", 1.72),
];

fn model_name(model: &str) -> String {
    model.trim().to_lowercase()
}

/// 文本模型使用的编码。
fn encoding_for_model(model: &str) -> &'static str {
    let name = model.trim().to_lowercase();
    // o200k_base：gpt-4o 及之后（含 gpt-5 系列、o1/o3/o4）
    if name.starts_with("gpt-5")
        || name.starts_with("gpt-4o")
        || name.starts_with("gpt-4.1")
        || name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4")
    {
        "o200k_base"
    } else {
        "cl100k_base"
    }
}

/// 取（并缓存）BPE 词表实例。
///
/// 词表在编译期嵌入二进制，首次调用时构建 —— 之后复用，不会重复解析。
fn bpe(name: &'static str) -> Option<&'static tiktoken_rs::CoreBPE> {
    static O200K: std::sync::OnceLock<Option<tiktoken_rs::CoreBPE>> = std::sync::OnceLock::new();
    static CL100K: std::sync::OnceLock<Option<tiktoken_rs::CoreBPE>> = std::sync::OnceLock::new();
    let cell = match name {
        "o200k_base" => &O200K,
        _ => &CL100K,
    };
    cell.get_or_init(|| {
        let built = if name == "o200k_base" {
            tiktoken_rs::o200k_base()
        } else {
            tiktoken_rs::cl100k_base()
        };
        match built {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(encoding = name, %error, "tiktoken 词表加载失败，改用字符数估算");
                None
            }
        }
    })
    .as_ref()
}

/// 计算一段文本的 token 数。
///
/// 按 ADR-010 用 `tiktoken-rs` **真实计算**，而不是估算 ——
/// `usage` 是对外契约，编造的数字比不返回更糟。
///
/// 词表不可用时退回「按字符数粗估」，并在日志里告警。这种情况不应发生，
/// 但宁可降级也不能让整个请求失败。
pub fn count_text_tokens(text: &str, model: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    if let Some(bpe) = bpe(encoding_for_model(model)) {
        return bpe.encode_ordinary(text).len() as i64;
    }
    // 降级路径：CJK 按 1 字符 ≈ 1 token，其余按 4 字符 ≈ 1 token
    let cjk = text
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count() as i64;
    let rest = text.chars().count() as i64 - cjk;
    cjk + (rest + 3) / 4
}

/// 计算 chat messages 的 prompt token 数。
///
/// 在文本 token 之外加上每条消息的结构开销（role、分隔符）与回复引导开销，
/// 与 OpenAI 公布的 chat 计量口径一致。
pub fn count_message_text_tokens(messages: &[(String, String)], model: &str) -> i64 {
    let mut total = 0i64;
    for (role, content) in messages {
        total += count_text_tokens(role, model);
        total += count_text_tokens(content, model);
        total += 3; // 每条消息的结构开销
    }
    total + 3 // 回复引导
}

/// 从图片字节读尺寸。无法识别时返回 `None`。
///
/// 从字节读图片尺寸（与 PIL `Image.open` 语义一致）。
pub fn image_size_from_bytes(data: &[u8]) -> Option<(u32, u32)> {
    if data.is_empty() {
        return None;
    }
    let reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// 从 data URL 读尺寸。
pub fn image_size_from_data_url(value: &str) -> Option<(u32, u32)> {
    let text = value.trim();
    let payload = match text.split_once(',') {
        Some((_, rest)) if text.starts_with("data:") => rest,
        _ => text,
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    image_size_from_bytes(&bytes)
}

/// 解析 `size` 字段（支持 `"1024x1024"` / `"1024*1024"` / 数组）。
///
pub fn parse_image_size(size: Option<&Value>) -> (u32, u32) {
    if let Some(value) = size {
        // 数组形式 [w, h]
        if let Some(items) = value.as_array() {
            if items.len() >= 2 {
                if let (Some(w), Some(h)) = (as_positive_int(&items[0]), as_positive_int(&items[1])) {
                    return (w, h);
                }
            }
        }
        // 字符串形式 "1024x1024"
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if let Some((w, h)) = parse_size_from_text(&text) {
            return (w, h);
        }
    }
    DEFAULT_IMAGE_SIZE
}

fn as_positive_int(value: &Value) -> Option<u32> {
    let n = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if n.is_finite() && n > 0.0 {
        Some(n as u32)
    } else {
        None
    }
}

/// 从字符串里抠尺寸，等价于正则 `(\\d{2,5})\\D+(\\d{2,5})`。
fn parse_size_from_text(text: &str) -> Option<(u32, u32)> {
    static SIZE_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(\d{2,5})\D+(\d{2,5})").expect("尺寸正则"));
    let caps = SIZE_RE.captures(text)?;
    let width: u32 = caps.get(1)?.as_str().parse().ok()?;
    let height: u32 = caps.get(2)?.as_str().parse().ok()?;
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

fn patch_count(width: f64, height: f64) -> i64 {
    let w = (width / PATCH_SIZE).ceil();
    let h = (height / PATCH_SIZE).ceil();
    (w * h) as i64
}

fn patch_multiplier(model: &str) -> f64 {
    let name = model_name(model);
    for (prefix, multiplier) in PATCH_MULTIPLIERS {
        if name.starts_with(prefix) {
            return *multiplier;
        }
    }
    1.0
}

fn patch_limits(model: &str, detail: &str) -> Option<(i64, f64)> {
    let name = model_name(model);
    if PATCH_1536_MODELS.iter().any(|p| name.starts_with(p)) {
        return Some((1536, 2048.0));
    }
    if name.starts_with("gpt-5.5") {
        return if detail == "auto" || detail == "original" {
            Some((10000, 6000.0))
        } else {
            Some((2500, 2048.0))
        };
    }
    if name.starts_with("gpt-5.4") {
        return if detail == "original" {
            Some((10000, 6000.0))
        } else {
            Some((2500, 2048.0))
        };
    }
    None
}

fn patch_tokens(width: u32, height: u32, model: &str, detail: &str) -> i64 {
    let multiplier = patch_multiplier(model);
    if detail == "low" {
        return (256.0 * multiplier).ceil() as i64;
    }

    let Some((patch_budget, max_dimension)) = patch_limits(model, detail) else {
        return 0;
    };
    let mut resized_width = width as f64;
    let mut resized_height = height as f64;
    let scale = (max_dimension / resized_width.max(resized_height)).min(1.0);
    resized_width *= scale;
    resized_height *= scale;

    let budget = patch_budget as f64;
    if patch_count(resized_width, resized_height) > patch_budget {
        let shrink =
            ((PATCH_SIZE * PATCH_SIZE * budget) / (resized_width * resized_height)).sqrt();
        let width_units = resized_width * shrink / PATCH_SIZE;
        let height_units = resized_height * shrink / PATCH_SIZE;
        let adj_width = if width_units != 0.0 {
            width_units.floor() / width_units
        } else {
            1.0
        };
        let adj_height = if height_units != 0.0 {
            height_units.floor() / height_units
        } else {
            1.0
        };
        let adjusted = shrink * adj_width.min(adj_height);
        resized_width *= adjusted;
        resized_height *= adjusted;
    }

    let tokens = patch_count(resized_width.max(1.0), resized_height.max(1.0)).min(patch_budget);
    (tokens as f64 * multiplier).ceil() as i64
}

fn tile_rates(model: &str) -> (i64, i64) {
    let name = model_name(model);
    if name == "gpt-5" || name == "gpt-5-chat-latest" {
        return (70, 140);
    }
    if name.starts_with("gpt-4o-mini") {
        return (2833, 5667);
    }
    if name.starts_with("o1") || name.starts_with("o3") {
        return (75, 150);
    }
    if name.starts_with("computer-use-preview") {
        return (65, 129);
    }
    (85, 170)
}

///
/// 注意：图片输入 token 用 patch 算法而非 tile 算法，
/// 本函数保留以备将来使用。
pub fn tile_tokens(width: u32, height: u32, model: &str, detail: &str) -> i64 {
    let (base_tokens, per_tile) = tile_rates(model);
    if detail == "low" {
        return base_tokens;
    }
    let mut w = width as f64;
    let mut h = height as f64;
    let scale = (2048.0 / w.max(h)).min(1.0);
    w *= scale;
    h *= scale;
    let short_side = w.min(h);
    if short_side > 0.0 {
        let upscale = TILE_HIGH_SHORT_SIDE / short_side;
        w *= upscale;
        h *= upscale;
    }
    let tiles = (w / TILE_SIZE).ceil() * (h / TILE_SIZE).ceil();
    base_tokens + tiles as i64 * per_tile
}

/// 估算一张输入图片的 token 数。
///
/// **`_model` 被有意忽略**，见模块文档。
///
pub fn count_image_input_tokens(width: u32, height: u32, _model: &str, detail: &str) -> i64 {
    if width == 0 || height == 0 {
        return 0;
    }
    let normalized = {
        let trimmed = detail.trim().to_lowercase();
        if trimmed.is_empty() {
            "auto".to_string()
        } else {
            trimmed
        }
    };
    patch_tokens(width, height, IMAGE_INPUT_TOKEN_MODEL, &normalized)
}

/// 估算一张生成图片的 token 数。
///
pub fn count_generated_image_tokens(width: u32, height: u32, quality: &str) -> i64 {
    let patches = patch_count(width as f64, height as f64);
    let quality = quality.trim().to_lowercase();
    match quality.as_str() {
        "low" => ((patches as f64) * 17.0 / 64.0).ceil() as i64,
        "high" | "hd" => ((patches as f64) * 65.0 / 16.0).ceil() as i64,
        _ => ((patches as f64) * 33.0 / 32.0).ceil() as i64,
    }
}

/// 估算 `count` 张同尺寸生成图片的 token 数。
pub fn count_image_output_tokens(size: Option<&Value>, quality: &str, count: i64) -> i64 {
    let (width, height) = parse_image_size(size);
    count.max(0) * count_generated_image_tokens(width, height, quality)
}

/// 估算一组生成结果（每项可能自带 `b64_json`）的 token 数。
pub fn count_image_output_items_tokens(
    items: &[Value],
    size: Option<&Value>,
    quality: &str,
) -> i64 {
    if items.is_empty() {
        return 0;
    }
    let fallback = parse_image_size(size);
    let mut total = 0i64;
    for item in items {
        let mut dims = None;
        if let Some(obj) = item.as_object() {
            if let Some(b64) = obj.get("b64_json").and_then(Value::as_str) {
                let b64 = b64.trim();
                if !b64.is_empty() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        dims = image_size_from_bytes(&bytes);
                    }
                }
            }
        }
        let (w, h) = dims.unwrap_or(fallback);
        total += count_generated_image_tokens(w, h, quality);
    }
    total
}

/// 构造 OpenAI 风格的 usage 对象。
///
pub fn token_usage(
    input_text_tokens: i64,
    input_image_tokens: i64,
    output_text_tokens: i64,
    output_image_tokens: i64,
) -> Value {
    let in_text = input_text_tokens.max(0);
    let in_image = input_image_tokens.max(0);
    let out_text = output_text_tokens.max(0);
    let out_image = output_image_tokens.max(0);
    let input_tokens = in_text + in_image;
    let output_tokens = out_text + out_image;
    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "input_tokens_details": {
            "text_tokens": in_text,
            "image_tokens": in_image,
            "cached_tokens": 0,
        },
        "output_tokens_details": {
            "text_tokens": out_text,
            "image_tokens": out_image,
            "reasoning_tokens": 0,
        },
    })
}

/// 图片接口的 usage 快捷构造。
pub fn image_usage(input_text_tokens: i64, input_image_tokens: i64, output_tokens: i64) -> Value {
    token_usage(input_text_tokens, input_image_tokens, 0, output_tokens)
}

/// 把 Responses 风格 usage 转成 Chat Completions 风格。
///
pub fn chat_usage_from_image_usage(usage: &Value) -> Value {
    let get = |path: &[&str]| -> i64 {
        let mut cur = usage;
        for key in path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => return 0,
            }
        }
        cur.as_i64().unwrap_or(0)
    };
    let input_tokens = get(&["input_tokens"]);
    let output_tokens = get(&["output_tokens"]);
    json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "prompt_tokens_details": {
            "text_tokens": get(&["input_tokens_details", "text_tokens"]),
            "image_tokens": get(&["input_tokens_details", "image_tokens"]),
            "cached_tokens": get(&["input_tokens_details", "cached_tokens"]),
        },
        "completion_tokens_details": {
            "text_tokens": get(&["output_tokens_details", "text_tokens"]),
            "image_tokens": get(&["output_tokens_details", "image_tokens"]),
            "reasoning_tokens": get(&["output_tokens_details", "reasoning_tokens"]),
        },
    })
}

#[cfg(test)]
mod text_token_tests {
    use super::*;

    // 下面这些数值是**实测**得来的（tiktoken-rs 0.9.1 + o200k_base / cl100k_base），
    // 不是估算。改动编码选择会让它们失败 —— 那正是它们存在的意义。

    #[test]
    fn english_words_match_tiktoken() {
        assert_eq!(count_text_tokens("hello world", "gpt-5-5"), 2);
        assert_eq!(count_text_tokens("hello world", "gpt-4"), 2);
        assert_eq!(
            count_text_tokens("The quick brown fox jumps over the lazy dog.", "gpt-5-5"),
            10
        );
    }

    #[test]
    fn chinese_is_counted_natively() {
        // 中文在 o200k_base 下约 1 字 1 token 上下，绝不是「4 字符 1 token」
        let text = "用一句话说明什么是 HTTP 状态码 404。不要用列表。";
        assert_eq!(count_text_tokens(text, "gpt-5-5"), 17);
    }

    #[test]
    fn empty_is_zero_and_single_char_is_one() {
        assert_eq!(count_text_tokens("", "gpt-5-5"), 0);
        assert_eq!(count_text_tokens("a", "gpt-5-5"), 1);
    }

    #[test]
    fn longer_text_costs_more() {
        let short = count_text_tokens("hello", "gpt-5-5");
        let long = count_text_tokens("hello hello hello hello", "gpt-5-5");
        assert!(long > short, "长文本 token 应更多：{long} vs {short}");
    }

    #[test]
    fn encodings_are_chosen_by_model() {
        assert_eq!(encoding_for_model("gpt-5-5"), "o200k_base");
        assert_eq!(encoding_for_model("gpt-4o-mini"), "o200k_base");
        assert_eq!(encoding_for_model("gpt-4.1"), "o200k_base");
        assert_eq!(encoding_for_model("o3-mini"), "o200k_base");
        assert_eq!(encoding_for_model("gpt-4"), "cl100k_base");
        assert_eq!(encoding_for_model("text-davinci-003"), "cl100k_base");
    }

    #[test]
    fn message_tokens_include_structure_overhead() {
        // 1(role) + 1(hello) + 3(每条开销) + 3(回复引导) = 8
        assert_eq!(
            count_message_text_tokens(&[("user".to_string(), "hello".to_string())], "gpt-5-5"),
            8
        );
        assert_eq!(count_message_text_tokens(&[], "gpt-5-5"), 3);
    }

    #[test]
    fn message_tokens_grow_with_message_count() {
        let one = count_message_text_tokens(&[("user".to_string(), "hi".to_string())], "gpt-5-5");
        let two = count_message_text_tokens(
            &[
                ("user".to_string(), "hi".to_string()),
                ("assistant".to_string(), "hi".to_string()),
            ],
            "gpt-5-5",
        );
        assert!(two > one, "两条消息应比一条贵：{two} vs {one}");
    }
}

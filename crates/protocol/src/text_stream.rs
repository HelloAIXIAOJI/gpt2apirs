//! 从上游 SSE 里还原助手文本。
//!
//! 上行把「完整消息」和「增量补丁」混在一起下发，必须只取用户可见的那条。
//! 四条过滤规则（缺一条就会泄漏内部内容）：
//!
//! 四条过滤规则，缺一条就泄漏：`role` 必须是 assistant（否则把用户的话当回复）；
//! `is_visually_hidden_from_conversation` 不能为 true（否则漏 system 消息）；
//! `recipient` 必须空或 `all`（否则漏发给工具的指令）；`channel` 必须空或 `final`（否则**漏思维链**）。

use serde_json::Value;

use crate::sse::iter_sse_payloads;

/// 内部注解的起始符（U+E200）。
const ANNOTATION_OPEN: char = '\u{E200}';
/// 内部注解的结束符（U+E201）。
const ANNOTATION_CLOSE: char = '\u{E201}';
/// 注解内部的分段符（U+E202）。
const ANNOTATION_SEP: char = '\u{E202}';

/// 判断某个 assistant 消息是否**面向用户**。
pub fn is_visible_assistant_message(message: &Value) -> bool {
    let Some(author) = message.get("author") else {
        return false;
    };
    let role = author
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if role != "assistant" {
        return false;
    }
    if message
        .get("metadata")
        .and_then(|meta| meta.get("is_visually_hidden_from_conversation"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return false;
    }
    let recipient = message
        .get("recipient")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !recipient.is_empty() && recipient != "all" {
        return false;
    }
    let channel = message
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !channel.is_empty() && channel != "final" {
        return false;
    }
    true
}

/// 取一条消息的正文。
///
/// 优先 `content.parts`（片段拼接）；`content_type: "code"` 时正文在 `content.text`。
pub fn assistant_message_text(message: &Value) -> String {
    if let Some(parts) = message
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        let joined: String = parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return joined;
        }
    }
    message
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 去掉前缀里的历史轮次内容。
///
/// 上游有时会把前几轮的文本一起重发，这里把已知历史从前缀剥掉。
pub fn strip_history(text: &str, history: &str) -> String {
    let mut remaining = text;
    while !history.is_empty() && remaining.starts_with(history) {
        remaining = &remaining[history.len()..];
    }
    remaining.to_string()
}

fn is_internal_annotation_part(part: &str) -> bool {
    let value = part.trim();
    if value.is_empty() {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    lower.starts_with("turn") || lower.starts_with("source") || lower.starts_with("sources")
}

fn readable_annotation_part(parts: &[&str]) -> String {
    for part in parts {
        let value = part.trim();
        if !value.is_empty() && !is_internal_annotation_part(value) {
            return value.to_string();
        }
    }
    String::new()
}

fn annotation_text(payload: &str) -> String {
    let parts: Vec<&str> = payload.split(ANNOTATION_SEP).collect();
    let kind = parts.first().map(|p| p.trim().to_ascii_lowercase()).unwrap_or_default();
    let data: Vec<&str> = if parts.len() > 1 { parts[1..].to_vec() } else { Vec::new() };
    if kind == "url" {
        let label = data.first().map(|p| p.trim()).unwrap_or("");
        let url = data.get(1).map(|p| p.trim()).unwrap_or("");
        if !label.is_empty() && (url.starts_with("http://") || url.starts_with("https://")) {
            return format!("{label} ({url})");
        }
        return if !label.is_empty() { label.to_string() } else { url.to_string() };
    }
    // cite 与其它类型都只取第一个可读片段
    readable_annotation_part(&data)
}

/// 清掉内部注解标记（U+E200…U+E201）。
///
/// 上游 web 端会用私用区字符下发引用标记，API 客户端渲染不了。
/// 这里保留可读标签（如链接文字），丢掉内部引用编号。
pub fn sanitize_output_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(ANNOTATION_OPEN) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + ANNOTATION_OPEN.len_utf8()..];
        let Some(end) = after_open.find(ANNOTATION_CLOSE) else {
            // 未闭合：一直删到结尾
            return out;
        };
        let payload = &after_open[..end];
        let mut tail = &after_open[end + ANNOTATION_CLOSE.len_utf8()..];
        let replacement = annotation_text(payload);
        // 紧跟标点时，前面的空白只在有替换内容时才保留
        let before_punctuation = tail
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
        if before_punctuation {
            let trimmed_len = out.trim_end().len();
            out.truncate(trimmed_len);
            if !replacement.is_empty() {
                out.push_str(&replacement);
                // 保留原本的空白（空白与替换内容一起处理）
            }
        } else {
            out.push_str(&replacement);
        }
        rest = tail;
        let _ = &mut tail;
    }
    out.push_str(rest);
    out
}

/// 应用一个补丁操作。
fn apply_patch_op(operation: &Value, current: &str, history: &str) -> String {
    let op = operation.get("o").and_then(Value::as_str).unwrap_or("");
    let value = operation
        .get("v")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    match op {
        "append" => format!("{current}{value}"),
        "replace" => strip_history(&value, history),
        _ => current.to_string(),
    }
}

/// 递归应用文本补丁。
pub fn apply_text_patch(event: &Value, current: &str, history: &str) -> String {
    if event.get("p").and_then(Value::as_str) == Some("/message/content/parts/0") {
        return apply_patch_op(event, current, history);
    }
    let operations = event.get("v");
    // 裸字符串增量：{"v": "一段文本"} 且没有 p / o
    if let Some(Value::String(text)) = operations {
        let has_path = event.get("p").is_some();
        let has_op = event.get("o").is_some();
        if !current.is_empty() && !has_path && !has_op {
            return format!("{current}{text}");
        }
        return current.to_string();
    }
    let Some(Value::Array(items)) = operations else {
        return current.to_string();
    };
    let mut text = current.to_string();
    for item in items {
        if item.is_object() {
            text = apply_text_patch(item, &text, history);
        }
    }
    text
}

/// 从一个事件里取「当前完整文本」。
///
/// 先看有没有完整消息（直接取），没有再按补丁累积。
pub fn assistant_raw_text(event: &Value, current: &str, history: &str) -> String {
    for candidate in [Some(event), event.get("v")].into_iter().flatten() {
        if !candidate.is_object() {
            continue;
        }
        if let Some(message) = candidate.get("message") {
            if message.is_object() && is_visible_assistant_message(message) {
                let text = assistant_message_text(message);
                if !text.is_empty() {
                    return strip_history(&text, history);
                }
            }
        }
    }
    apply_text_patch(event, current, history)
}

/// 文本累积器：喂 SSE 事件，吐出增量。
#[derive(Debug, Default, Clone)]
pub struct TextAccumulator {
    /// 未清洗的完整文本。
    pub raw: String,
    /// 已清洗的完整文本。
    pub text: String,
    /// 会话 ID（一旦出现就记住）。
    pub conversation_id: String,
    /// 是否见过 DONE。
    pub done: bool,
}

impl TextAccumulator {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一条 SSE payload（`data:` 后面的原文）。
    ///
    /// 返回**新增**的文本片段；没有新增返回 `None`。
    pub fn push_payload(&mut self, payload: &str, history: &str) -> Option<String> {
        if payload.trim() == "[DONE]" {
            self.done = true;
            return None;
        }
        if self.conversation_id.is_empty() {
            if let Some(found) = find_conversation_id(payload) {
                self.conversation_id = found;
            }
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return None;
        };
        let next_raw = assistant_raw_text(&event, &self.raw, history);
        if next_raw == self.raw {
            return None;
        }
        let next_text = sanitize_output_text(&next_raw);
        let delta = if next_text.starts_with(&self.text) {
            next_text[self.text.len()..].to_string()
        } else {
            next_text.clone()
        };
        self.raw = next_raw;
        self.text = next_text;
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }

    /// 一次性吃掉整段 SSE 文本。
    pub fn push_raw_sse(&mut self, raw: &str, history: &str) -> Vec<String> {
        let mut deltas = Vec::new();
        for payload in iter_sse_payloads(raw) {
            if let Some(delta) = self.push_payload(&payload, history) {
                deltas.push(delta);
            }
        }
        deltas
    }
}

/// 从任意 payload 里抠 `conversation_id`。
fn find_conversation_id(payload: &str) -> Option<String> {
    let key = "\"conversation_id\"";
    let index = payload.find(key)?;
    let rest = &payload[index + key.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 便捷函数：整段 SSE  (文本, conversation_id, 增量序列)。
pub fn collect_text(raw_sse: &str, history: &str) -> (String, String, Vec<String>) {
    let mut accumulator = TextAccumulator::new();
    let deltas = accumulator.push_raw_sse(raw_sse, history);
    (accumulator.text, accumulator.conversation_id, deltas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_message(text: &str) -> Value {
        json!({
            "v": { "message": {
                "author": { "role": "assistant" },
                "content": { "content_type": "text", "parts": [text] },
                "recipient": "all",
                "channel": "final",
            }}
        })
    }

    #[test]
    fn visible_message_rules() {
        assert!(is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "content": {"parts": ["x"]},
        })));

        // 用户消息
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "user"},
        })));
        // 视觉隐藏
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "metadata": {"is_visually_hidden_from_conversation": true},
        })));
        // 发给工具的
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "recipient": "web",
        })));
        // 非 final 通道（思维链）
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "channel": "analysis",
        })));
        // 空 recipient/channel 是允许的
        assert!(is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "recipient": "",
            "channel": "",
        })));
    }

    #[test]
    fn message_text_prefers_parts_then_text_field() {
        let message = json!({"content": {"parts": ["a", "b"]}});
        assert_eq!(assistant_message_text(&message), "ab");
        let message = json!({"content": {"content_type": "code", "text": "code here"}});
        assert_eq!(assistant_message_text(&message), "code here");
        assert_eq!(assistant_message_text(&json!({"content": {}})), "");
    }

    #[test]
    fn strip_history_removes_prefix() {
        assert_eq!(strip_history("abcXYZ", "abc"), "XYZ");
        assert_eq!(strip_history("XYZ", "abc"), "XYZ");
        assert_eq!(strip_history("abc", ""), "abc");
        // 反复剥离
        assert_eq!(strip_history("abcabcX", "abc"), "X");
    }

    #[test]
    fn append_patch_accumulates() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"o": "patch", "v": [
            {"p": "/message/content/parts/0", "o": "append", "v": "你好"}
        ]});
        assert_eq!(accumulator.push_payload(&event.to_string(), ""), Some("你好".to_string()));
        let event = json!({"o": "patch", "v": [
            {"p": "/message/content/parts/0", "o": "append", "v": "，世界"}
        ]});
        assert_eq!(accumulator.push_payload(&event.to_string(), ""), Some("，世界".to_string()));
        assert_eq!(accumulator.text, "你好，世界");
    }

    #[test]
    fn replace_patch_overwrites() {
        let mut accumulator = TextAccumulator::new();
        accumulator.push_payload(
            &json!({"o": "patch", "v": [{"p": "/message/content/parts/0", "o": "append", "v": "旧"}]})
                .to_string(),
            "",
        );
        accumulator.push_payload(
            &json!({"o": "patch", "v": [{"p": "/message/content/parts/0", "o": "replace", "v": "新"}]})
                .to_string(),
            "",
        );
        assert_eq!(accumulator.text, "新");
    }

    #[test]
    fn bare_string_delta_appends_after_seed() {
        let mut accumulator = TextAccumulator::new();
        // 裸字符串增量只在「已有基础文本」时追加：
        // 首条通常是完整消息，之后的裸串才是增量。
        accumulator.push_payload(&assistant_message("第一段").to_string(), "");
        // 真实形态是 {"v": "增量"}，不是顶层 JSON 字符串
        let delta = accumulator.push_payload(&json!({"v": "第二段"}).to_string(), "");
        assert_eq!(delta, Some("第二段".to_string()));
        assert_eq!(accumulator.text, "第一段第二段");
    }

    #[test]
    fn bare_string_before_any_text_is_ignored() {
        // 没有基础文本时，裸字符串无法判断是「增量」还是「新消息」，保守忽略
        let mut accumulator = TextAccumulator::new();
        assert_eq!(
            accumulator.push_payload(&json!({"v": "孤立增量"}).to_string(), ""),
            None
        );
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn full_message_event_is_taken_directly() {
        let mut accumulator = TextAccumulator::new();
        let delta = accumulator.push_payload(&assistant_message("完整回复").to_string(), "");
        assert_eq!(delta, Some("完整回复".to_string()));
        assert_eq!(accumulator.text, "完整回复");
    }

    #[test]
    fn history_prefix_is_stripped() {
        let mut accumulator = TextAccumulator::new();
        let delta = accumulator.push_payload(&assistant_message("历史新内容").to_string(), "历史");
        assert_eq!(delta, Some("新内容".to_string()));
    }

    #[test]
    fn hidden_message_does_not_leak() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"v": {"message": {
            "author": {"role": "assistant"},
            "content": {"parts": ["系统内部说明"]},
            "metadata": {"is_visually_hidden_from_conversation": true},
        }}});
        assert_eq!(accumulator.push_payload(&event.to_string(), ""), None);
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn reasoning_channel_does_not_leak() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"v": {"message": {
            "author": {"role": "assistant"},
            "content": {"parts": ["思维链内容"]},
            "channel": "analysis",
        }}});
        assert_eq!(accumulator.push_payload(&event.to_string(), ""), None);
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn conversation_id_is_captured() {
        let mut accumulator = TextAccumulator::new();
        accumulator.push_payload(
            &json!({"type": "resume_conversation_token", "conversation_id": "conv-123"}).to_string(),
            "",
        );
        assert_eq!(accumulator.conversation_id, "conv-123");
    }

    #[test]
    fn done_payload_marks_finished() {
        let mut accumulator = TextAccumulator::new();
        assert_eq!(accumulator.push_payload("[DONE]", ""), None);
        assert!(accumulator.done);
    }

    #[test]
    fn whole_sse_stream_is_reduced_to_text() {
        let sse = concat!(
            "data: \"v1\"\n\n",
            "data: {\"conversation_id\":\"c-1\"}\n\n",
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"Hel\"}]}\n\n",
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"lo\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (text, conversation_id, deltas) = collect_text(sse, "");
        assert_eq!(text, "Hello");
        assert_eq!(conversation_id, "c-1");
        assert_eq!(deltas, vec!["Hel".to_string(), "lo".to_string()]);
    }

    #[test]
    fn annotation_markers_are_cleaned() {
        // \u{E200}url\u{E202}标签\u{E202}https://x.y\u{E201}
        let raw = format!(
            "见这里{ANNOTATION_OPEN}url{ANNOTATION_SEP}来源{ANNOTATION_SEP}https://example.com{ANNOTATION_CLOSE}。"
        );
        let cleaned = sanitize_output_text(&raw);
        assert!(cleaned.contains("来源 (https://example.com)"), "{cleaned}");
        assert!(!cleaned.contains(ANNOTATION_OPEN), "标记应被移除: {cleaned}");
    }

    #[test]
    fn internal_annotation_is_dropped() {
        // 内部引用编号（turn0search1）应被丢弃
        let raw = format!(
            "结论{ANNOTATION_OPEN}cite{ANNOTATION_SEP}turn0search1{ANNOTATION_CLOSE}成立"
        );
        let cleaned = sanitize_output_text(&raw);
        assert_eq!(cleaned, "结论成立", "{cleaned}");
    }

    #[test]
    fn unclosed_annotation_truncates_tail() {
        let raw = format!("正文{ANNOTATION_OPEN}cite{ANNOTATION_SEP}turn0");
        assert_eq!(sanitize_output_text(&raw), "正文");
    }

    #[test]
    fn plain_text_is_untouched() {
        let text = "完全没有标记的一句话。";
        assert_eq!(sanitize_output_text(text), text);
    }
}

//! 语音：文本转语音（TTS）与语音转文字（STT）。
//!
//! 上游两个端点：
//! - `POST /backend-api/transcribe`（multipart 上传音频）→ `{text}`
//! - `GET  /backend-api/synthesize?conversation_id&message_id&voice&format` → 音频字节
//!
//! synthesize **只能合成已存在的消息**，所以要先把文字变成一条 assistant 消息
//! （见 `generate::speech::speak` 的两跳实现）。

use serde_json::Value;

/// 上游接受的音色名。
///
/// 官网的九个（juniper…vale）与 OpenAI 的九个（alloy…shimmer）
/// `/backend-api/synthesize` 都认，所以原样透传，不做映射。
pub const VOICES: &[&str] = &[
    "juniper", "maple", "cove", "breeze", "orbit", "glimmer", "ember", "fathom", "vale",
    "alloy", "ash", "coral", "echo", "fable", "onyx", "nova", "sage", "shimmer",
];

/// 默认音色。
pub const DEFAULT_VOICE: &str = "cove";

/// 归一音色名；空串用默认。
pub fn resolve_voice(name: &str) -> Result<String, String> {
    let voice = name.trim().to_ascii_lowercase();
    if voice.is_empty() {
        return Ok(DEFAULT_VOICE.to_string());
    }
    if VOICES.contains(&voice.as_str()) {
        return Ok(voice);
    }
    Err(format!(
        "不支持的音色 {name}；可用音色：{}",
        VOICES.join(", ")
    ))
}

/// 归一输出格式（官方支持 mp3 / opus / aac / flac / wav）。
pub fn resolve_format(name: &str) -> String {
    let format = name.trim().to_ascii_lowercase();
    let picked = match format.as_str() {
        "opus" => "opus",
        "aac" => "aac",
        "flac" => "flac",
        "wav" => "wav",
        _ => "mp3",
    };
    picked.to_string()
}

/// 音频格式对应的 Content-Type。
pub fn audio_content_type(format: &str) -> &'static str {
    match format {
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        _ => "audio/mpeg",
    }
}

/// 从转写响应里取文本。
pub fn transcript(body: &Value) -> Option<String> {
    body.get("text")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
}

/// 从会话详情里找**最后一条** assistant 消息的 id。
///
/// synthesize 要的就是这个 id；拿不到就没法合成。
///
/// 上游有**两种**结构，都要认：
/// - 新格式：顶层 `messages` 数组，每条直接是消息对象（当前线上就是这个）
/// - 旧格式：`mapping` 对象，值是 `{message: {...}}` 的节点
pub fn latest_assistant_message_id(detail: &Value) -> Option<String> {
    if let Some(items) = detail.get("messages").and_then(Value::as_array) {
        let best = items
            .iter()
            .filter_map(newest_assistant)
            .max_by(|left, right| left.0.total_cmp(&right.0));
        if let Some((_, id)) = best {
            return Some(id);
        }
    }
    let mapping = detail.get("mapping")?.as_object()?;
    let mut best: Option<(f64, String)> = None;
    for node in mapping.values() {
        let Some(message) = node.get("message") else {
            continue;
        };
        let role = message
            .get("author")
            .and_then(|author| author.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            continue;
        };
        let created = message
            .get("create_time")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if best.as_ref().map(|(seen, _)| created > *seen).unwrap_or(true) {
            best = Some((created, id.to_string()));
        }
    }
    best.map(|(_, id)| id)
}

/// 一条消息如果是 assistant，返回 `(create_time, id)`。
fn newest_assistant(message: &Value) -> Option<(f64, String)> {
    let role = message
        .get("author")
        .and_then(|author| author.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if role != "assistant" {
        return None;
    }
    let id = message.get("id").and_then(Value::as_str)?.to_string();
    let created = message
        .get("create_time")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Some((created, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_voice_falls_back_to_default() {
        assert_eq!(resolve_voice("").unwrap(), DEFAULT_VOICE);
        assert_eq!(resolve_voice("   ").unwrap(), DEFAULT_VOICE);
    }

    #[test]
    fn both_naming_schemes_are_accepted() {
        // 官网音色与 OpenAI 音色上游都认，大小写也不计较
        assert_eq!(resolve_voice("NOVA").unwrap(), "nova");
        assert_eq!(resolve_voice("juniper").unwrap(), "juniper");
    }

    #[test]
    fn unknown_voice_is_rejected_with_the_list() {
        let error = resolve_voice("bogus").unwrap_err();
        assert!(error.contains("bogus"), "{error}");
        assert!(error.contains("cove"), "要告诉用户有哪些可选：{error}");
    }

    #[test]
    fn format_falls_back_to_mp3() {
        assert_eq!(resolve_format(""), "mp3");
        assert_eq!(resolve_format("FLAC"), "flac");
        assert_eq!(resolve_format("whatever"), "mp3");
        assert_eq!(audio_content_type("wav"), "audio/wav");
        assert_eq!(audio_content_type("mp3"), "audio/mpeg");
    }

    #[test]
    fn transcript_trims_text() {
        let body = json!({ "text": "  你好 world  " });
        assert_eq!(transcript(&body).as_deref(), Some("你好 world"));
        assert_eq!(transcript(&json!({})), None);
    }

    #[test]
    fn picks_the_newest_assistant_message() {
        let detail = json!({
            "mapping": {
                "a": { "message": { "id": "m1", "author": { "role": "assistant" }, "create_time": 100.0 } },
                "b": { "message": { "id": "m2", "author": { "role": "assistant" }, "create_time": 300.0 } },
                "c": { "message": { "id": "u1", "author": { "role": "user" }, "create_time": 400.0 } }
            }
        });
        assert_eq!(latest_assistant_message_id(&detail).as_deref(), Some("m2"));
    }

    /// 线上现在是新格式：顶层 `messages` 数组，没有 `mapping`。
    #[test]
    fn reads_messages_array_format() {
        let detail = json!({
            "messages": [
                { "id": "u1", "author": { "role": "user" }, "create_time": 10.0 },
                { "id": "m1", "author": { "role": "assistant" }, "create_time": 20.0 },
                { "id": "m2", "author": { "role": "assistant" }, "create_time": 30.0 }
            ]
        });
        assert_eq!(latest_assistant_message_id(&detail).as_deref(), Some("m2"));
    }

    #[test]
    fn missing_assistant_message_is_none() {
        let detail = json!({ "mapping": { "a": { "message": { "id": "u1", "author": { "role": "user" } } } } });
        assert_eq!(latest_assistant_message_id(&detail), None);
        assert_eq!(latest_assistant_message_id(&json!({})), None);
    }
}

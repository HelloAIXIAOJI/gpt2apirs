//! 上游 SSE 流解析与对话状态提取。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// 对话状态，随 SSE 流增量更新。
#[derive(Debug, Default, Clone)]
pub struct ConversationState {
    /// 对话 ID。
    pub conversation_id: String,
    /// 生成的图片文件 ID（不含输入图）。
    pub file_ids: Vec<String>,
    /// sediment 资源 ID（图片实际落在这里）。
    pub sediment_ids: Vec<String>,
    /// 是否被内容策略拦截。
    pub blocked: bool,
    /// 上游是否真的调用了工具（`server_ste_metadata.tool_invoked`）。
    pub tool_invoked: Option<bool>,
    /// 本轮用途，例如 `"image gen"`。
    pub turn_use_case: String,
    /// 累积的助手文本。
    ///
    /// 生图链路里这段文本**只有在上游没给图时才有意义**：模型拒绝或解释时
    /// 只有文本、没有图片 id。出图后附带的说明文字也会进这里，所以判断
    /// 「是否只是回了文本」必须先确认没有图片资产。
    pub text: String,
    /// 产出 `text` 的累积器（喂事件时同步更新，调用方不用管）。
    assistant: crate::text_stream::TextAccumulator,
}

/// 从一条 SSE 事件得到的增量结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationEventKind {
    /// 文本增量。
    Delta(String),
    /// 其他事件（含原始 payload 的类型名，便于日志）。
    Other(String),
    /// 流结束。
    Done,
}

/// 一条解析后的事件。
#[derive(Debug, Clone)]
pub struct ConversationEvent {
    /// 事件类型。
    pub kind: ConversationEventKind,
    /// 该事件处理后的完整状态快照。
    pub state: ConversationState,
}
fn re_conversation_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""conversation_id"\s*:\s*"([^"]+)""#).expect("conversation_id 正则"))
}

fn re_file_service_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"file-service://([A-Za-z0-9_-]+)").expect("file-service 正则"))
}

fn re_real_image_file_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bfile_00000000[a-f0-9]{24}\b").expect("真实图片 ID 正则"))
}

fn re_sediment_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"sediment://([A-Za-z0-9_-]+)").expect("sediment 正则"))
}

fn push_unique(target: &mut Vec<String>, candidate: &str) {
    if !candidate.is_empty() && !target.iter().any(|item| item == candidate) {
        target.push(candidate.to_string());
    }
}

/// 从原始 payload 字符串里提取 `(conversation_id, file_ids, sediment_ids)`。
///
pub fn extract_conversation_ids(payload: &str) -> (String, Vec<String>, Vec<String>) {
    let conversation_id = re_conversation_id()
        .captures(payload)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    let mut file_ids = Vec::new();
    for caps in re_file_service_id().captures_iter(payload) {
        if let Some(m) = caps.get(1) {
            push_unique(&mut file_ids, m.as_str());
        }
    }
    for m in re_real_image_file_id().find_iter(payload) {
        push_unique(&mut file_ids, m.as_str());
    }

    let mut sediment_ids = Vec::new();
    for caps in re_sediment_id().captures_iter(payload) {
        if let Some(m) = caps.get(1) {
            push_unique(&mut sediment_ids, m.as_str());
        }
    }

    (conversation_id, file_ids, sediment_ids)
}

/// 事件是否来自 user 角色的消息（用于排除输入图）。
fn is_user_message_event(event: &Value) -> bool {
    let message = event.get("message").or_else(|| {
        event
            .get("v")
            .filter(|v| v.is_object())
            .and_then(|v| v.get("message"))
    });
    let Some(message) = message else {
        return false;
    };
    message
        .get("author")
        .and_then(|a| a.get("role"))
        .and_then(Value::as_str)
        .map(|role| role.trim().eq_ignore_ascii_case("user"))
        .unwrap_or(false)
}

/// 事件是否是完整的图片工具消息。
fn is_image_tool_event(event: &Value) -> bool {
    let message = event.get("message").or_else(|| {
        event
            .get("v")
            .filter(|v| v.is_object())
            .and_then(|v| v.get("message"))
    });
    let Some(message) = message else {
        return false;
    };
    let role = message
        .get("author")
        .and_then(|a| a.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if role != "tool" {
        return false;
    }
    let metadata = message.get("metadata");
    if metadata
        .and_then(|m| m.get("async_task_type"))
        .and_then(Value::as_str)
        == Some("image_gen")
    {
        return true;
    }
    let content = message.get("content");
    let content_type = content
        .and_then(|c| c.get("content_type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if content_type != "multimodal_text" {
        return false;
    }
    content
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts.iter().any(|part| {
                if !part.is_object() {
                    return false;
                }
                let is_pointer = part
                    .get("content_type")
                    .and_then(Value::as_str)
                    == Some("image_asset_pointer");
                let has_pointer_prefix = part
                    .get("asset_pointer")
                    .and_then(Value::as_str)
                    .map(|p| p.starts_with("file-service://") || p.starts_with("sediment://"))
                    .unwrap_or(false);
                is_pointer || has_pointer_prefix
            })
        })
        .unwrap_or(false)
}

/// 用一条原始 payload 更新状态。
///
pub fn update_conversation_state(state: &mut ConversationState, payload: &str) {
    // 助手正文先落在累积器里：拒绝/解释类的回复只有文本，没有图片 id，
    // 不留下来就只能对外报一个「没图」的空错误。
    state.assistant.push_payload(payload);
    if !state.assistant.text.is_empty() {
        state.text = state.assistant.text.clone();
    }

    let parsed: Option<Value> = serde_json::from_str(payload).ok();
    let event = parsed.as_ref().filter(|v| v.is_object());

    let (conversation_id, file_ids, sediment_ids) = extract_conversation_ids(payload);
    if state.conversation_id.is_empty() && !conversation_id.is_empty() {
        state.conversation_id = conversation_id;
    }

    let is_patch_event = event
        .and_then(|e| e.get("o"))
        .and_then(Value::as_str)
        == Some("patch");
    let is_user_msg = event.map(is_user_message_event).unwrap_or(false);
    let image_context = event.map(is_image_tool_event).unwrap_or(false)
        || (state.tool_invoked == Some(true) && !is_user_msg)
        || (is_patch_event
            && !is_user_msg
            && (payload.contains("asset_pointer") || payload.contains("file-service://")));

    if image_context {
        for id in &file_ids {
            push_unique(&mut state.file_ids, id);
        }
        for id in &sediment_ids {
            push_unique(&mut state.sediment_ids, id);
        }
    }

    let Some(event) = event else {
        return;
    };

    if let Some(cid) = event.get("conversation_id").and_then(Value::as_str) {
        if !cid.is_empty() {
            state.conversation_id = cid.to_string();
        }
    }
    if let Some(value) = event.get("v").filter(|v| v.is_object()) {
        if let Some(cid) = value.get("conversation_id").and_then(Value::as_str) {
            if !cid.is_empty() {
                state.conversation_id = cid.to_string();
            }
        }
    }

    match event.get("type").and_then(Value::as_str) {
        Some("moderation") => {
            if let Some(moderation) = event.get("moderation_response") {
                if moderation.get("blocked").and_then(Value::as_bool) == Some(true) {
                    state.blocked = true;
                }
            }
        }
        Some("server_ste_metadata") => {
            if let Some(metadata) = event.get("metadata") {
                if let Some(flag) = metadata.get("tool_invoked").and_then(Value::as_bool) {
                    state.tool_invoked = Some(flag);
                }
                if let Some(use_case) = metadata.get("turn_use_case").and_then(Value::as_str) {
                    if !use_case.is_empty() {
                        state.turn_use_case = use_case.to_string();
                    }
                }
            }
        }
        _ => {}
    }
}

/// 从 SSE 原始文本中拆出 `data:` 载荷。
///
/// 切出 SSE payload：只取 `data:` 开头的行，去掉前缀后 trim。
pub fn iter_sse_payloads(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if !payload.is_empty() {
            out.push(payload.to_string());
        }
    }
    out
}

/// 解析整条 SSE 原始文本，返回逐事件结果与最终状态。
///
pub fn parse_conversation_stream(raw: &str) -> (Vec<ConversationEvent>, ConversationState) {
    let mut state = ConversationState::default();
    let mut events = Vec::new();

    for payload in iter_sse_payloads(raw) {
        if payload == "[DONE]" {
            events.push(ConversationEvent {
                kind: ConversationEventKind::Done,
                state: state.clone(),
            });
            break;
        }
        update_conversation_state(&mut state, &payload);
        let kind = match serde_json::from_str::<Value>(&payload) {
            Ok(value) if value.is_object() => {
                let label = value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| "patch-or-data".to_string());
                ConversationEventKind::Other(label)
            }
            Ok(_) => ConversationEventKind::Other("non-object".to_string()),
            Err(_) => ConversationEventKind::Other("raw".to_string()),
        };
        events.push(ConversationEvent {
            kind,
            state: state.clone(),
        });
    }

    // 收尾：整条流只有一条终态回复（拒绝/极短回答）时它被结构判定跳过了，
    // 到这里才认 —— 否则生图链路会拿不到那段拒绝理由。
    if state.assistant.flush().is_some() {
        state.text = state.assistant.text.clone();
    }

    (events, state)
}


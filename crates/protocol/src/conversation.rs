//! 会话请求体构造与响应解析（**纯逻辑，无 HTTP**）。
//!
//! `_prepare_image_conversation` 与 `_start_image_generation`。
//!
//! # 这些字段为什么长这样
//!
//! 上游会校验一批「浏览器上下文」字段（时区、屏幕尺寸、暗色模式等）。
//! 缺字段不一定失败，但**字段明显不真实会提高风控概率**。
//! 这里的取值来自真实抓包，不要随意改。

use serde_json::{Value, json};

use crate::util::new_uuid as util_uuid;

/// 默认上游模型 slug。
pub const DEFAULT_MODEL: &str = "gpt-5-5";

/// 一张已经上传到上游的参考图。
///
/// 图生图时上游只认 `file_id`，所以必须先走上传（`POST /backend-api/files` 三步）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// 上游文件 ID。
    pub file_id: String,
    /// 文件名。
    pub file_name: String,
    /// MIME。
    pub mime_type: String,
    /// 字节数。
    pub size_bytes: usize,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

/// 把参考图转成消息 `parts` 里的 `image_asset_pointer` 片段。
pub fn asset_pointer_part(image: &ImageReference) -> Value {
    json!({
        "content_type": "image_asset_pointer",
        "asset_pointer": format!("file-service://{}", image.file_id),
        "width": image.width,
        "height": image.height,
        "size_bytes": image.size_bytes,
    })
}

/// 把参考图转成消息 `metadata.attachments` 条目。
pub fn attachment_entry(image: &ImageReference) -> Value {
    json!({
        "id": image.file_id,
        "mimeType": image.mime_type,
        "name": image.file_name,
        "size": image.size_bytes,
        "width": image.width,
        "height": image.height,
    })
}

/// 会话准备（拿 conduit_token）的请求体。
pub fn build_conversation_prepare_payload(prompt: &str, model: &str) -> Value {
    json!({
        "action": "next",
        "fork_from_shared_post": false,
        "parent_message_id": util_uuid(),
        "model": model,
        "client_prepare_state": "success",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "system_hints": ["picture_v2"],
        "partial_query": {
            "id": util_uuid(),
            "author": { "role": "user" },
            "content": { "content_type": "text", "parts": [prompt] },
        },
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": { "app_name": "chatgpt.com" },
    })
}

/// 发起会话（SSE）的请求体。
///
/// `images` 非空时走多模态分支：把参考图作为 `image_asset_pointer` 放在 prompt 之前，
/// 同时在 `metadata.attachments` 里再报一次（上游两处都读）。
pub fn build_conversation_payload(
    prompt: &str,
    model: &str,
    created: f64,
    images: &[ImageReference],
) -> Value {
    let (content, attachments) = if images.is_empty() {
        (json!({ "content_type": "text", "parts": [prompt] }), None)
    } else {
        let mut parts: Vec<Value> = images.iter().map(asset_pointer_part).collect();
        parts.push(Value::String(prompt.to_string()));
        let attachments: Vec<Value> = images.iter().map(attachment_entry).collect();
        (
            json!({ "content_type": "multimodal_text", "parts": parts }),
            Some(attachments),
        )
    };
    let mut metadata = json!({
        "developer_mode_connector_ids": [],
        "selected_github_repos": [],
        "selected_all_github_repos": false,
        "system_hints": ["picture_v2"],
        "serialization_metadata": { "custom_symbol_offsets": [] },
    });
    if let Some(attachments) = attachments {
        metadata["attachments"] = Value::Array(attachments);
    }
    json!({
        "action": "next",
        "messages": [{
            "id": util_uuid(),
            "author": { "role": "user" },
            "create_time": created,
            "content": content,
            "metadata": metadata,
        }],
        "parent_message_id": util_uuid(),
        "model": model,
        "client_prepare_state": "sent",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "enable_message_followups": true,
        "system_hints": ["picture_v2"],
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": {
            "is_dark_mode": false,
            "time_since_loaded": 1200,
            "page_height": 1072,
            "page_width": 1724,
            "pixel_ratio": 1.2,
            "screen_height": 1440,
            "screen_width": 2560,
            "app_name": "chatgpt.com",
        },
        "paragen_cot_summary_display_override": "allow",
        "force_parallel_switch": "auto",
    })
}

/// 文本会话（SSE）的请求体。
///
/// 与图片链路的三处关键差异：
///
/// 文本与图片的请求差异：`system_hints` 分别传空 / `["picture_v2"]`；
/// 文本要 `history_and_training_disabled: true`；图片要 `X-Conduit-Token`。
///
/// 另外文本链路**不走** `conversation/prepare`。
pub fn build_text_conversation_payload(prompt: &str, model: &str) -> Value {
    build_text_conversation_payload_messages(
        &[("user".to_string(), prompt.to_string())],
        model,
    )
}

/// 多轮版本的文本会话请求体。
///
/// 上游接受任意条 message，所以多轮对话把历史一并送上去，由上游自行处理上下文。
/// 角色只透传字符串，不做映射 —— 上游认 `system` / `user` / `assistant`。
pub fn build_text_conversation_payload_messages(
    messages: &[(String, String)],
    model: &str,
) -> Value {
    let converted: Vec<Value> = messages
        .iter()
        .map(|(role, text)| {
            json!({
                "id": util_uuid(),
                "author": { "role": role },
                "content": { "content_type": "text", "parts": [text] },
            })
        })
        .collect();
    json!({
        "action": "next",
        "messages": converted,
        "model": model,
        "parent_message_id": util_uuid(),
        "conversation_mode": { "kind": "primary_assistant" },
        "conversation_origin": null,
        "force_paragen": false,
        "force_paragen_model_slug": "",
        "force_rate_limit": false,
        "force_use_sse": true,
        "history_and_training_disabled": true,
        "reset_rate_limits": false,
        "suggestions": [],
        "supported_encodings": [],
        "system_hints": [],
        "timezone": "Asia/Shanghai",
        "timezone_offset_min": -480,
        "variant_purpose": "comparison_implicit",
        "websocket_request_id": util_uuid(),
        "client_contextual_info": {
            "is_dark_mode": false,
            "time_since_loaded": 120,
            "page_height": 900,
            "page_width": 1400,
            "pixel_ratio": 2,
            "screen_height": 1440,
            "screen_width": 2560,
        },
    })
}

/// 从 prepare 响应里取 conduit_token。
pub fn parse_conduit_token(body: &Value) -> Option<String> {
    body.get("conduit_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}


// 会话详情（GET /backend-api/conversation/{id}）

/// 会话详情。[`parse_conversation_detail`] 的解析结果。
#[derive(Debug, Clone, Default)]
pub struct ConversationDetail {
    /// 会话 ID。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 消息，按 `create_time` 升序。
    pub messages: Vec<ConversationMessage>,
}

/// 会话里的一条消息。
#[derive(Debug, Clone, Default)]
pub struct ConversationMessage {
    /// 消息 ID。
    pub id: String,
    /// 角色：user / assistant / system / tool。
    pub role: String,
    /// 正文（多条 part 已拼接）。
    pub text: String,
    /// 创建时间（Unix 秒，上游给的是浮点）。
    pub create_time: f64,
    /// 结束状态，如 `finished_successfully`。
    pub status: String,
    /// 是否为界面上不显示的消息（system / 工具指令等）。
    pub hidden: bool,
}

/// 解析 `GET /backend-api/conversation/{id}` 的响应。
///
/// 上游把消息放在 `mapping` 里（节点 → `message`），顺序不保证，
/// 所以解析后按 `create_time` 升序排列；缺失字段一律给空值，不猜内容。
pub fn parse_conversation_detail(payload: &Value) -> ConversationDetail {
    let detail_id = payload
        .get("id")
        .or_else(|| payload.get("conversation_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut messages = Vec::new();
    if let Some(mapping) = payload.get("mapping").and_then(Value::as_object) {
        for node in mapping.values() {
            let Some(message) = node.get("message") else {
                continue;
            };
            if !message.is_object() {
                continue;
            }
            let metadata = message.get("metadata");
            let status = metadata
                .and_then(|value| value.get("finish_details"))
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                .or_else(|| {
                    metadata
                        .and_then(|value| value.get("status"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_string();
            messages.push(ConversationMessage {
                id: message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                role: message
                    .get("author")
                    .and_then(|value| value.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: crate::text_stream::assistant_message_text(message),
                create_time: message
                    .get("create_time")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
                status,
                hidden: metadata
                    .and_then(|value| value.get("is_visually_hidden_from_conversation"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    messages.sort_by(|a, b| a.create_time.total_cmp(&b.create_time));

    ConversationDetail {
        id: detail_id,
        title,
        messages,
    }
}

/// 最后一条**可见的**助手消息。
///
/// 轮询会话时拿最终答案用它：不可见的（system / 工具指令）不算答案。
pub fn latest_assistant_message(detail: &ConversationDetail) -> Option<&ConversationMessage> {
    detail
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant" && !message.hidden && !message.text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_payload_has_no_image_hints() {
        let payload = build_text_conversation_payload("你好", "gpt-5-5");
        assert_eq!(
            payload["system_hints"].as_array().map(Vec::len),
            Some(0),
            "文本链路不能带 picture_v2"
        );
        assert_eq!(payload["history_and_training_disabled"], true);
        assert_eq!(payload["force_use_sse"], true);
        assert_eq!(payload["timezone"], "Asia/Shanghai");
        assert_eq!(payload["messages"][0]["content"]["parts"][0], "你好");
        assert_eq!(payload["messages"][0]["author"]["role"], "user");
        assert!(payload["websocket_request_id"].is_string());
    }

    #[test]
    fn text_payload_ids_differ_per_call() {
        let a = build_text_conversation_payload("x", "m");
        let b = build_text_conversation_payload("x", "m");
        assert_ne!(a["parent_message_id"], b["parent_message_id"]);
        assert_ne!(a["websocket_request_id"], b["websocket_request_id"]);
    }

    #[test]
    fn prepare_payload_has_required_fields() {
        let payload = build_conversation_prepare_payload("a cat", DEFAULT_MODEL);
        assert_eq!(payload["action"], "next");
        assert_eq!(payload["model"], DEFAULT_MODEL);
        assert_eq!(payload["system_hints"][0], "picture_v2");
        assert_eq!(payload["partial_query"]["content"]["parts"][0], "a cat");
        assert_eq!(payload["timezone"], "Asia/Shanghai");
        assert!(payload["parent_message_id"].is_string());
    }

    fn sample_ref() -> ImageReference {
        ImageReference {
            file_id: "file-abc".to_string(),
            file_name: "image_1.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 277,
            width: 64,
            height: 48,
        }
    }

    #[test]
    fn text_only_payload_has_no_attachments() {
        let payload = build_conversation_payload("hi", DEFAULT_MODEL, 1.0, &[]);
        assert_eq!(
            payload["messages"][0]["content"]["content_type"],
            "text"
        );
        assert_eq!(payload["messages"][0]["content"]["parts"][0], "hi");
        assert!(
            payload["messages"][0]["metadata"].get("attachments").is_none(),
            "纯文本不应带 attachments"
        );
    }

    #[test]
    fn multimodal_payload_puts_image_before_prompt() {
        let payload = build_conversation_payload("edit this", DEFAULT_MODEL, 1.0, &[sample_ref()]);
        let content = &payload["messages"][0]["content"];
        assert_eq!(content["content_type"], "multimodal_text");
        assert_eq!(content["parts"][0]["content_type"], "image_asset_pointer");
        assert_eq!(content["parts"][0]["asset_pointer"], "file-service://file-abc");
        assert_eq!(content["parts"][0]["width"], 64);
        assert_eq!(content["parts"][0]["size_bytes"], 277);
        assert_eq!(content["parts"][1], "edit this", "prompt 必须在图片之后");
    }

    #[test]
    fn multimodal_payload_reports_attachments_too() {
        let payload = build_conversation_payload("edit this", DEFAULT_MODEL, 1.0, &[sample_ref()]);
        let attachments = &payload["messages"][0]["metadata"]["attachments"];
        assert!(attachments.is_array(), "多模态应带 attachments");
        assert_eq!(attachments[0]["id"], "file-abc");
        assert_eq!(attachments[0]["mimeType"], "image/png");
        assert_eq!(attachments[0]["name"], "image_1.png");
        assert_eq!(attachments[0]["size"], 277);
        assert_eq!(attachments[0]["width"], 64);
        assert_eq!(attachments[0]["height"], 48);
    }

    #[test]
    fn multiple_images_all_appear() {
        let mut second = sample_ref();
        second.file_id = "file-def".to_string();
        let payload =
            build_conversation_payload("edit", DEFAULT_MODEL, 1.0, &[sample_ref(), second]);
        let parts = payload["messages"][0]["content"]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts.len(), 3, "两张图 + prompt");
        assert_eq!(parts[2], "edit");
        let attachments = payload["messages"][0]["metadata"]["attachments"]
            .as_array()
            .expect("attachments");
        assert_eq!(attachments.len(), 2);
    }

    #[test]
    fn conversation_payload_has_required_fields() {
        let payload = build_conversation_payload("a dog", DEFAULT_MODEL, 1_700_000_000.5, &[]);
        assert_eq!(payload["action"], "next");
        assert_eq!(payload["messages"][0]["content"]["parts"][0], "a dog");
        assert_eq!(payload["messages"][0]["create_time"], 1_700_000_000.5);
        assert_eq!(payload["system_hints"][0], "picture_v2");
        assert_eq!(payload["client_contextual_info"]["screen_width"], 2560);
        assert!(payload["messages"][0]["metadata"]["system_hints"][0] == "picture_v2");
    }

    #[test]
    fn ids_are_unique_per_call() {
        let a = build_conversation_prepare_payload("x", DEFAULT_MODEL);
        let b = build_conversation_prepare_payload("x", DEFAULT_MODEL);
        assert_ne!(a["parent_message_id"], b["parent_message_id"]);
        assert_ne!(a["partial_query"]["id"], b["partial_query"]["id"]);
    }

    #[test]
    fn conduit_token_parsing() {
        assert_eq!(
            parse_conduit_token(&json!({ "conduit_token": " abc "})).as_deref(),
            Some("abc")
        );
        assert_eq!(parse_conduit_token(&json!({ "conduit_token": "" })), None);
        assert_eq!(parse_conduit_token(&json!({})), None);
    }

    fn sample_detail() -> serde_json::Value {
        serde_json::json!({
            "id": "conv-abc",
            "title": "关于哈希",
            "mapping": {
                "n2": {
                    "message": {
                        "id": "m2",
                        "author": { "role": "assistant" },
                        "content": { "content_type": "text", "parts": ["哈希是把任意长度输入映射成定长摘要。"] },
                        "create_time": 100.5,
                        "metadata": { "finish_details": { "type": "finished_successfully" } }
                    }
                },
                "n0": {
                    "message": {
                        "id": "m0",
                        "author": { "role": "system" },
                        "content": { "content_type": "text", "parts": ["内部指令"] },
                        "create_time": 100.0,
                        "metadata": { "is_visually_hidden_from_conversation": true }
                    }
                },
                "n1": {
                    "message": {
                        "id": "m1",
                        "author": { "role": "user" },
                        "content": { "content_type": "text", "parts": ["什么是哈希"] },
                        "create_time": 100.2,
                        "metadata": {}
                    }
                },
                "root": { "message": null, "parent": null }
            }
        })
    }

    #[test]
    fn detail_sorts_messages_by_time() {
        let detail = parse_conversation_detail(&sample_detail());
        assert_eq!(detail.id, "conv-abc");
        assert_eq!(detail.title, "关于哈希");
        let roles: Vec<&str> = detail.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant"], "应按 create_time 升序");
    }

    #[test]
    fn detail_marks_hidden_messages() {
        let detail = parse_conversation_detail(&sample_detail());
        let hidden: Vec<&str> = detail
            .messages
            .iter()
            .filter(|m| m.hidden)
            .map(|m| m.role.as_str())
            .collect();
        assert_eq!(hidden, vec!["system"]);
    }

    #[test]
    fn latest_assistant_message_skips_hidden_and_empty() {
        let detail = parse_conversation_detail(&sample_detail());
        let message = latest_assistant_message(&detail).expect("应找到助手消息");
        assert_eq!(message.role, "assistant");
        assert!(message.text.contains("定长摘要"));
    }

    #[test]
    fn detail_survives_missing_fields() {
        let detail = parse_conversation_detail(&serde_json::json!({}));
        assert!(detail.id.is_empty());
        assert!(detail.messages.is_empty());

        // 无 mapping 之外还要能吃 mapping 里非对象节点
        let detail = parse_conversation_detail(&serde_json::json!({
            "id": "x", "mapping": { "a": 1, "b": { "message": "not-an-object" } }
        }));
        assert_eq!(detail.id, "x");
        assert!(detail.messages.is_empty());
    }

    /// 真实抓包的脱敏副本（2026-11-12 采集）。
    ///
    /// 结构 100% 来自上游，长文本与图片 base64 已截断；
    /// 用它锁住「真实响应形状」而不是我们自己造的结构。
    fn real_detail_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/conversation_detail.json"))
            .expect("夹具应是合法 JSON")
    }

    #[test]
    fn parses_real_conversation_fixture() {
        let detail = parse_conversation_detail(&real_detail_fixture());
        assert!(!detail.id.is_empty(), "应从 conversation_id 取到 ID");
        assert!(!detail.title.trim().is_empty(), "应有标题");
        assert!(detail.messages.len() >= 3, "真实会话至少 3 条消息");

        // create_time 升序
        let times: Vec<f64> = detail.messages.iter().map(|m| m.create_time).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(times, sorted, "消息应按时间升序");

        // 角色齐全：user / assistant / tool
        for role in ["user", "assistant", "tool"] {
            assert!(
                detail.messages.iter().any(|m| m.role == role),
                "真实会话里应有 {role} 消息"
            );
        }

        // 隐藏消息被标出来
        assert!(
            detail.messages.iter().any(|m| m.hidden),
            "真实会话里存在界面上不显示的消息"
        );
    }

    #[test]
    fn latest_message_from_real_fixture_is_usable() {
        let detail = parse_conversation_detail(&real_detail_fixture());
        let message = latest_assistant_message(&detail).expect("应取到可见助手消息");
        assert_eq!(message.role, "assistant");
        assert!(!message.hidden, "不应选到隐藏消息");
        assert!(!message.text.trim().is_empty(), "正文不应为空");
    }
}

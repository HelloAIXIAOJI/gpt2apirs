//! `POST /v1/chat/completions` —— OpenAI 兼容文本对话。
//!
//! 与生图的三点差别：允许 `limited` 账号（文本不吃图片配额）；记账走 `mark_text_result`（**不扣**配额）；
//! token 用 tiktoken 真实计算而不是估算。
//!
//! 流式是**真流式**：握手后 `post_sse_stream` 边收边转，每拿到一个完整 SSE 事件就吐一个片段
//! （见 `generate::text::generate_text_stream`）；首字延迟不再等于总生成时间。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use gpt2apirs_core::AppError;

use crate::error::ApiError;
use crate::routes::text_common::{StreamHandle, run_text, start_text_stream};
use crate::state::AppState;

/// 一条聊天消息。
#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    /// 角色：`system` / `user` / `assistant`。
    pub role: String,
    /// 内容：字符串，或 `[{"type":"text","text":"..."}]`。
    pub content: Value,
}

/// `POST /v1/chat/completions` 的请求体。
///
/// 未列出的字段（`temperature` / `top_p` / `max_tokens` 等）接受但忽略 ——
/// 上游 web 会话不暴露这些旋钮，**假装支持会让用户以为调了有用**。
#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    /// 模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// 消息列表。
    pub messages: Vec<ChatMessage>,
    /// 是否流式。
    #[serde(default)]
    pub stream: Option<bool>,
    /// 工具列表（**不支持**，非空即拒）。
    #[serde(default)]
    pub tools: Option<Value>,
}

/// 把一条消息的 `content` 取成纯文本。
fn message_text(message: &ChatMessage) -> Result<String, ApiError> {
    match &message.content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut collected = String::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" | "input_text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            collected.push_str(text);
                        }
                    }
                    "image_url" | "image" | "input_image" => {
                        return Err(ApiError::openai(AppError::invalid_param(
                            "文本端点不支持图片输入；请用 /v1/images/edits",
                            "messages",
                        )));
                    }
                    _ => {}
                }
            }
            Ok(collected)
        }
        other => Err(ApiError::openai(AppError::invalid_param(
            format!("content 类型不支持：{other}"),
            "messages",
        ))),
    }
}

fn normalize_role(role: &str) -> Result<String, ApiError> {
    let normalized = role.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "system" | "user" | "assistant" => Ok(normalized),
        other => Err(ApiError::openai(AppError::invalid_param(
            format!("不支持的角色 {other:?}；只接受 system / user / assistant"),
            "messages",
        ))),
    }
}

/// 关键词粗筛。
///
/// 说明清楚它在做什么：**这是关键词匹配，不是分类器**。
/// 列表由设置项 `chat_blocked_keywords`（逗号分隔）提供，默认为空 —— 默认不做任何拦截。
/// 上游本身也有内容策略，会在生成阶段拒绝。
fn screen_content(settings: &gpt2apirs_core::settings::RuntimeSettings, texts: &[String]) -> Result<(), ApiError> {
    let raw = settings.get_string("chat_blocked_keywords");
    let blocked: Vec<String> = raw
        .split(',')
        .map(|item| item.trim().to_lowercase())
        .filter(|item| !item.is_empty())
        .collect();
    if blocked.is_empty() {
        return Ok(());
    }
    for text in texts {
        let lowered = text.to_lowercase();
        for keyword in &blocked {
            if lowered.contains(keyword) {
                tracing::warn!(keyword = %keyword, "命中内容粗筛关键词");
                return Err(ApiError::openai(AppError::InvalidRequest {
                    message: "请求被内容策略拒绝".to_string(),
                    param: None,
                    code: Some("content_policy_violation".to_string()),
                }));
            }
        }
    }
    Ok(())
}

/// `POST /v1/chat/completions`
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    if crate::routes::text_common::has_tools(&request.tools) {
        return Err(ApiError::openai(
            crate::routes::text_common::tools_not_supported("chat.tools"),
        ));
    }
    if request.messages.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "messages 不能为空",
            "messages",
        )));
    }

    // ---- 归一化 ----
    let mut messages: Vec<(String, String)> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let role = normalize_role(&message.role)?;
        let text = message_text(message)?;
        messages.push((role, text));
    }
    let has_content = messages.iter().any(|(_, text)| !text.trim().is_empty());
    if !has_content {
        return Err(ApiError::openai(AppError::invalid_param(
            "messages 里没有非空文本内容",
            "messages",
        )));
    }

    // ---- 内容粗筛（只在 chat 路径，见 ADR-010）----
    let user_texts: Vec<String> = messages
        .iter()
        .filter(|(role, _)| role != "assistant")
        .map(|(_, text)| text.clone())
        .collect();
    screen_content(&state.settings, &user_texts)?;

    let requested_model = request
        .model
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "gpt-5-5".to_string());
    let deadline = gpt2apirs_generate::default_request_deadline(&state.settings);

    if request.stream.unwrap_or(false) {
        // ---- 真流式：上游每吐一段就转发一段 ----
        let handle = start_text_stream(
            &state,
            messages.clone(),
            &requested_model,
            deadline,
        )
        .await?;
        return Ok(build_live_stream(handle, &requested_model).into_response());
    }

    // ---- 取号  生成  记账（与其他文本端点共用）----
    let outcome = run_text(&state, &messages, &requested_model, deadline).await?;
    let result = &outcome.result;
    let diag = &outcome.diag;

    let usage = build_usage(&messages, &result.text, &requested_model);
    let created = chrono::Utc::now().timestamp();
    let completion_id = format!("chatcmpl-{}", Uuid::new_v4().simple());

    tracing::info!(
        account = %outcome.account_id,
        model = %requested_model,
        conversation_id = %result.conversation_id,
        prompt_tokens = usage["prompt_tokens"].as_i64().unwrap_or(0),
        completion_tokens = usage["completion_tokens"].as_i64().unwrap_or(0),
        sse_bytes = diag.sse_bytes,
        cached = outcome.cached,
        "文本生成完成"
    );

    Ok(Json(json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": result.text },
            "finish_reason": "stop",
        }],
        "usage": usage,
    }))
    .into_response())
}

fn build_usage(messages: &[(String, String)], reply: &str, model: &str) -> Value {
    let (prompt_tokens, completion_tokens) = crate::routes::text_common::token_counts(messages, reply, model);
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

fn build_live_stream(handle: StreamHandle, model: &str) -> axum::response::Response {
    use axum::response::sse::{Event, Sse};
    use futures_util::StreamExt;

    let completion_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model = model.to_string();
    let snapshot = handle.snapshot.clone();
    let messages = handle.billing_messages.clone();
    let last_error: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));

    // 1) role 块
    let head_id = completion_id.clone();
    let head_model = model.clone();
    let head = futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().data(chunk_payload(
            &head_id,
            &head_model,
            created,
            json!({ "role": "assistant", "content": "" }),
            Value::Null,
            None,
        )))
    });

    // 2) 正文块
    let body_id = completion_id.clone();
    let body_model = model.clone();
    let body_errors = last_error.clone();
    let body = handle.deltas.filter_map(move |item| {
        let id = body_id.clone();
        let model = body_model.clone();
        let errors = body_errors.clone();
        async move {
            match item {
                Ok(delta) => Some(Ok(Event::default().data(chunk_payload(
                    &id,
                    &model,
                    created,
                    json!({ "content": delta }),
                    Value::Null,
                    None,
                )))),
                Err(error) => {
                    // 记下来，末块里报出去；不在这里中断，让客户端拿到已生成的部分
                    if let Ok(mut guard) = errors.lock() {
                        *guard = Some(error.to_string());
                    }
                    None
                }
            }
        }
    });

    // 3) 末块（含 usage 与错误说明）
    let tail_id = completion_id.clone();
    let tail_model = model.clone();
    let tail_snapshot = snapshot;
    let tail_messages = messages;
    let tail_errors = last_error;
    let tail = futures_util::stream::once(async move {
        let text = tail_snapshot
            .lock()
            .map(|guard| guard.text.clone())
            .unwrap_or_default();
        let usage = build_usage(&tail_messages, &text, &tail_model);
        let mut payload: Value = serde_json::from_str(&chunk_payload(
            &tail_id,
            &tail_model,
            created,
            json!({}),
            json!("stop"),
            Some(usage),
        ))
        .unwrap_or_else(|_| json!({}));
        if let Some(error) = tail_errors.lock().ok().and_then(|guard| guard.clone()) {
            // OpenAI 的流里没有「错误块」的标准位置，这里用扩展字段说明
            payload["error"] = json!({ "message": error, "type": "upstream_error" });
        }
        Ok::<_, std::convert::Infallible>(Event::default().data(payload.to_string()))
    });

    // 4) [DONE]
    let done = futures_util::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
    });

    Sse::new(head.chain(body).chain(tail).chain(done)).into_response()
}

fn chunk_payload(
    completion_id: &str,
    model: &str,
    created: i64,
    delta: Value,
    finish_reason: Value,
    usage: Option<Value>,
) -> String {
    let mut payload = json!({
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    if let Some(usage) = usage {
        payload["usage"] = usage;
    }
    payload.to_string()
}

//! `POST /v1/messages` —— Anthropic Messages API。
//!
//! 与 OpenAI 的差异都在契约形态：`system` 是顶层字段；消息只允许 user / assistant；
//! 输出是 `content[]` 块数组；`max_tokens` 必填；错误体是 `{"type":"error","error":{...}}`。
//! 详见 docs/m4-text-endpoints-verification.md。

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use gpt2apirs_generate::default_request_deadline;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::text_common::{StreamHandle, resolve_model, run_text, start_text_stream};
use crate::state::AppState;

/// 一条 Anthropic 消息。
#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    /// `user` 或 `assistant`。
    pub role: String,
    /// 字符串，或 `[{type:"text",text}]`。
    pub content: Value,
}

/// `POST /v1/messages` 的请求体。
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    /// 模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// **必填**（Anthropic 规范），但本实现无法强制执行长度，只做存在性校验。
    pub max_tokens: i64,
    /// 系统提示：字符串，或 `[{type:"text",text}]`。
    #[serde(default)]
    pub system: Option<Value>,
    /// 消息列表。
    pub messages: Vec<AnthropicMessage>,
    /// 是否流式。
    #[serde(default)]
    pub stream: Option<bool>,
    /// 工具（只有 `model: text_tool` 接受）。
    #[serde(default)]
    pub tools: Option<Value>,
    /// 工具选择策略。
    #[serde(default)]
    pub tool_choice: Option<Value>,
}

/// `POST /v1/messages/count_tokens` 的请求体。
///
/// 与 [`MessagesRequest`] 的区别：`max_tokens` 不必填（Anthropic 规范如此），
/// 其余字段含义一致。
#[derive(Debug, Deserialize)]
pub struct CountTokensRequest {
    /// 模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// 系统提示：字符串，或 `[{type:"text",text}]`。
    #[serde(default)]
    pub system: Option<Value>,
    /// 消息列表。
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    /// 工具（**不支持**，与 `/v1/messages` 一致）。
    #[serde(default)]
    pub tools: Option<Value>,
}

fn anthropic_error(error: impl Into<gpt2apirs_core::AppError>) -> ApiError {
    ApiError::anthropic(error)
}

fn block_content(
    value: &Value,
    param: &str,
) -> Result<(String, Vec<gpt2apirs_generate::inputs::ImageSource>), ApiError> {
    match value {
        Value::Null => Ok((String::new(), Vec::new())),
        Value::String(text) => Ok((text.clone(), Vec::new())),
        Value::Array(parts) => {
            let mut out = String::new();
            let mut images = Vec::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    "image" => {
                        images.push(anthropic_image_source(part, param)?);
                    }
                    "image_url" => {
                        images.push(anthropic_image_source(part, param)?);
                    }
                    _ => {}
                }
            }
            Ok((out, images))
        }
        other => Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
            format!("content 类型不支持：{other}"),
            param,
        ))),
    }
}

/// 从一个 Anthropic 图片块里取出图片来源。
///
/// 官方两种写法：`source.type = base64`（带 media_type 与 data）与 `source.type = url`。
fn anthropic_image_source(
    part: &Value,
    param: &str,
) -> Result<gpt2apirs_generate::inputs::ImageSource, ApiError> {
    use base64::Engine as _;
    use gpt2apirs_generate::inputs::ImageSource;

    let source = part.get("source");
    let kind = source
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("base64");
    match kind {
        "url" => {
            let url = source
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    anthropic_error(gpt2apirs_core::AppError::invalid_param(
                        "图片 url 为空",
                        param,
                    ))
                })?;
            Ok(ImageSource::RemoteUrl(url.to_string()))
        }
        _ => {
            let data = source
                .and_then(|value| value.get("data"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anthropic_error(gpt2apirs_core::AppError::invalid_param(
                        "图片缺少 data",
                        param,
                    ))
                })?;
            let mime = source
                .and_then(|value| value.get("media_type"))
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|_| {
                    anthropic_error(gpt2apirs_core::AppError::invalid_param(
                        "图片 data 不是合法 base64",
                        param,
                    ))
                })?;
            Ok(ImageSource::Inline {
                data: bytes,
                filename: String::new(),
                mime,
            })
        }
    }
}

/// 把顶层 `system` 与消息列表归一成 `(role, text)`。
///
/// `/v1/messages` 与 `/v1/messages/count_tokens` 共用：两边的 token 口径必须一致，
/// 否则「预估能放下、实际被截断」。
fn normalize_parts(
    system: Option<&Value>,
    messages: &[AnthropicMessage],
) -> Result<(Vec<(String, String)>, Vec<gpt2apirs_generate::inputs::ImageSource>), ApiError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut images = Vec::new();
    if let Some(system) = system {
        let (text, _) = block_content(system, "system")?;
        if !text.trim().is_empty() {
            out.push(("system".to_string(), text));
        }
    }
    for message in messages {
        let role = message.role.trim().to_ascii_lowercase();
        match role.as_str() {
            "user" | "assistant" => {}
            other => {
                return Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
                    format!("不支持的角色 {other:?}；Anthropic 只接受 user / assistant（系统提示放顶层 system）"),
                    "messages",
                )));
            }
        }
        let (text, mut found) = block_content(&message.content, "messages")?;
        images.append(&mut found);
        if !text.trim().is_empty() {
            out.push((role, text));
        }
    }
    Ok((out, images))
}

/// `POST /v1/messages`
pub async fn create_message(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MessagesRequest>,
) -> Result<Response, ApiError> {
    if request.max_tokens <= 0 {
        return Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
            "max_tokens 必须为正整数",
            "max_tokens",
        )));
    }
    if request.messages.is_empty() {
        return Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
            "messages 不能为空",
            "messages",
        )));
    }

    let (messages, images) = normalize_parts(request.system.as_ref(), &request.messages)?;
    if messages.iter().all(|(_, text)| text.trim().is_empty()) && images.is_empty() {
        return Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
            "messages 里没有非空文本",
            "messages",
        )));
    }

    let model = resolve_model(request.model.as_deref());
    let deadline = default_request_deadline(&state.settings);

    // ---- 工具调用（只有 text_tool 允许）----
    let tools = crate::tool_call::parse_tools(request.tools.as_ref().unwrap_or(&Value::Null));
    if !tools.is_empty() && !gpt2apirs_generate::models::is_tool_model(&model) {
        return Err(ApiError::anthropic(gpt2apirs_core::AppError::InvalidRequest {
            message: format!(
                "模型 {model} 不支持工具调用；要用请把 model 改成 {}",
                gpt2apirs_generate::models::EXPOSED_TEXT_TOOL_MODEL
            ),
            param: Some("tools".to_string()),
            code: Some("tools_not_supported".to_string()),
        }));
    }
    let tool_prompt = if tools.is_empty() {
        None
    } else {
        Some(crate::tool_call::build_tool_prompt(
            &tools,
            crate::tool_call::requires_tool_call(request.tool_choice.as_ref().unwrap_or(&Value::Null)),
        ))
    };

    // 真流式：命中时直接返回 SSE，不走「先跑完再切块」
    if request.stream.unwrap_or(false) {
        let handle = start_text_stream(
            &state,
            messages.clone(),
            images.clone(),
            &model,
            deadline,
            None,
            tool_prompt.as_deref(),
        )
        .await?;
        if tool_prompt.is_some() {
            return Ok(build_tool_stream(handle, &model).into_response());
        }
        return Ok(build_live_stream(handle, &model).into_response());
    }

    let outcome = run_text(
        &state,
        &messages,
        &images,
        &model,
        deadline,
        None,
        tool_prompt.as_deref(),
    )
    .await?;

    let input_tokens = gpt2apirs_core::tokens::count_message_text_tokens(&messages, &model);
    let output_tokens = gpt2apirs_core::tokens::count_text_tokens(&outcome.result.text, &model);
    let message_id = format!("msg_{}", Uuid::new_v4().simple());

    tracing::info!(
        account = %outcome.account_id,
        model = %model,
        conversation_id = %outcome.result.conversation_id,
        input_tokens,
        output_tokens,
        sse_bytes = outcome.diag.sse_bytes,
        "anthropic messages 生成完成"
    );

    // ---- 工具调用：解析失败就当普通文本 ----
    let (clean_text, calls) = if tool_prompt.is_some() {
        crate::tool_call::parse_tool_calls(&outcome.result.text)
    } else {
        (outcome.result.text.clone(), Vec::new())
    };
    let mut content: Vec<Value> = Vec::new();
    if !clean_text.trim().is_empty() {
        content.push(json!({ "type": "text", "text": clean_text }));
    }
    content.extend(crate::tool_call::anthropic_tool_uses(&calls));
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }
    let stop_reason = if calls.is_empty() { "end_turn" } else { "tool_use" };

    Ok(Json(json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    }))
    .into_response())
}

/// `POST /v1/messages/count_tokens`
///
/// 只算不取号：不查账号池、不写调用日志的账号字段、不消耗配额。
/// 口径与 `/v1/messages` 的 `usage.input_tokens` 完全一致（同一个函数）。
pub async fn count_tokens(
    Json(request): Json<CountTokensRequest>,
) -> Result<Json<Value>, ApiError> {
    if crate::routes::text_common::has_tools(&request.tools) {
        return Err(ApiError::anthropic(
            crate::routes::text_common::tools_not_supported("anthropic.tools"),
        ));
    }
    if request.messages.is_empty() {
        return Err(anthropic_error(gpt2apirs_core::AppError::invalid_param(
            "messages 不能为空",
            "messages",
        )));
    }

    let (messages, _images) = normalize_parts(request.system.as_ref(), &request.messages)?;
    let model = resolve_model(request.model.as_deref());
    let input_tokens = gpt2apirs_core::tokens::count_message_text_tokens(&messages, &model);

    tracing::debug!(model = %model, input_tokens, "anthropic count_tokens");
    Ok(Json(json!({ "input_tokens": input_tokens })))
}

/// 把流式 handle 变成 axum SSE 响应（Anthropic 事件格式）。
///
/// 事件顺序：`message_start` → `content_block_start` → 若干
/// `content_block_delta` → `content_block_stop` →（可选 `error`）→
/// `message_delta` → `message_stop`。
fn build_live_stream(handle: StreamHandle, model: &str) -> axum::response::Response {
    use axum::response::sse::Sse;
    use futures_util::StreamExt;
    use std::convert::Infallible;

    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let model = model.to_string();
    let snapshot = handle.snapshot.clone();
    let messages = handle.billing_messages.clone();
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let input_tokens =
        gpt2apirs_core::tokens::count_message_text_tokens(&handle.billing_messages, &model);

    // 1) 开头两件
    let head = futures_util::stream::iter(vec![
        Ok::<_, Infallible>(sse_event(
            "message_start",
            json!({
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": input_tokens, "output_tokens": 0 },
                },
            }),
        )),
        Ok::<_, Infallible>(sse_event(
            "content_block_start",
            json!({ "index": 0, "content_block": { "type": "text", "text": "" } }),
        )),
    ]);

    // 2) 正文增量：来一段转一段
    let body_errors = last_error.clone();
    let body = handle.deltas.filter_map(move |item| {
        let errors = body_errors.clone();
        async move {
            match item {
                // Anthropic 的思考块要带 signature，我们没法伪造，这里只转正文
                Ok(gpt2apirs_protocol::text_stream::Delta::Text(delta)) => {
                    Some(Ok::<_, Infallible>(sse_event(
                        "content_block_delta",
                        json!({
                            "index": 0,
                            "delta": { "type": "text_delta", "text": delta },
                        }),
                    )))
                }
                Ok(gpt2apirs_protocol::text_stream::Delta::Reasoning(_)) => None,
                Err(error) => {
                    // 记下来在末尾报，不中断——客户端已收到的部分不该丢
                    if let Ok(mut slot) = errors.lock() {
                        *slot = Some(error.to_string());
                    }
                    None
                }
            }
        }
    });

    // 3) 收尾：output_tokens 只有全文出来后才能算
    let tail_model = model.clone();
    let tail_snapshot = snapshot;
    let tail_messages = messages;
    let tail_errors = last_error;
    let tail = futures_util::stream::once(async move {
        let text = tail_snapshot
            .lock()
            .map(|guard| guard.text.clone())
            .unwrap_or_default();
        let (_, output_tokens) =
            crate::routes::text_common::token_counts(&tail_messages, &text, &tail_model);

        let mut events = Vec::with_capacity(4);
        events.push(sse_event(
            "content_block_stop",
            json!({ "index": 0 }),
        ));
        if let Some(error) = tail_errors.lock().ok().and_then(|guard| guard.clone()) {
            events.push(sse_event(
                "error",
                json!({ "type": "error", "error": { "type": "api_error", "message": error } }),
            ));
        }
        events.push(sse_event(
            "message_delta",
            json!({
                "delta": { "stop_reason": "end_turn", "stop_sequence": Value::Null },
                "usage": { "output_tokens": output_tokens },
            }),
        ));
        events.push(sse_event("message_stop", json!({})));
        futures_util::stream::iter(events.into_iter().map(Ok::<_, Infallible>))
    })
    .flatten();

    Sse::new(head.chain(body).chain(tail))
        .keep_alive(super::sse_keep_alive())
        .into_response()
}

/// 组一条 SSE 事件：Anthropic 用 `event:` 字段区分类型。
/// 带工具时的流式（Anthropic 格式）：全文出来再解析，按 content block 发。
///
/// 文本块与工具块分开：`tool_use` 的入参用 `input_json_delta` 一次性发完。
fn build_tool_stream(handle: StreamHandle, model: &str) -> axum::response::Response {
    use axum::response::sse::Sse;
    use futures_util::StreamExt;
    use std::convert::Infallible;

    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let model = model.to_string();
    let snapshot = handle.snapshot.clone();
    let billing = handle.billing_messages.clone();
    let input_tokens = gpt2apirs_core::tokens::count_message_text_tokens(&billing, &model);

    let head_message_id = message_id.clone();
    let head_model = model.clone();
    let head = futures_util::stream::once(async move {
        Ok::<_, Infallible>(sse_event(
            "message_start",
            json!({
                "message": {
                    "id": head_message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": head_model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": input_tokens, "output_tokens": 0 },
                },
            }),
        ))
    });

    let drain = handle
        .deltas
        .filter_map(|_| async { None::<Result<_, Infallible>> });

    let tail = futures_util::stream::once(async move {
        let text = snapshot
            .lock()
            .map(|guard| guard.text.clone())
            .unwrap_or_default();
        let (clean, calls) = crate::tool_call::parse_tool_calls(&text);
        let output_tokens = gpt2apirs_core::tokens::count_text_tokens(&text, &model);

        let mut events: Vec<Result<_, Infallible>> = Vec::new();
        let mut index = 0usize;
        if !clean.trim().is_empty() {
            events.push(Ok(sse_event(
                "content_block_start",
                json!({ "index": index, "content_block": { "type": "text", "text": "" } }),
            )));
            events.push(Ok(sse_event(
                "content_block_delta",
                json!({ "index": index, "delta": { "type": "text_delta", "text": clean } }),
            )));
            events.push(Ok(sse_event(
                "content_block_stop",
                json!({ "index": index }),
            )));
            index += 1;
        }
        for block in crate::tool_call::anthropic_tool_uses(&calls) {
            let id = block["id"].clone();
            let name = block["name"].clone();
            let input = block["input"].to_string();
            events.push(Ok(sse_event(
                "content_block_start",
                json!({
                    "index": index,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
                }),
            )));
            events.push(Ok(sse_event(
                "content_block_delta",
                json!({
                    "index": index,
                    "delta": { "type": "input_json_delta", "partial_json": input },
                }),
            )));
            events.push(Ok(sse_event(
                "content_block_stop",
                json!({ "index": index }),
            )));
            index += 1;
        }
        let stop_reason = if calls.is_empty() { "end_turn" } else { "tool_use" };
        events.push(Ok(sse_event(
            "message_delta",
            json!({
                "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                "usage": { "output_tokens": output_tokens },
            }),
        )));
        events.push(Ok(sse_event("message_stop", json!({}))));
        futures_util::stream::iter(events)
    })
    .flatten();

    Sse::new(head.chain(drain).chain(tail))
        .keep_alive(super::sse_keep_alive())
        .into_response()
}

fn sse_event(kind: &str, mut payload: Value) -> axum::response::sse::Event {
    if let Value::Object(map) = &mut payload {
        map.insert("type".to_string(), json!(kind));
    }
    axum::response::sse::Event::default()
        .event(kind)
        .data(payload.to_string())
}

#[cfg(test)]
mod live_stream_tests {
    use std::time::Duration;

    use super::*;
    use crate::routes::test_support::{assert_before, channel_handle, collect_body};
    use futures_util::StreamExt;
    use gpt2apirs_generate::error::GenerateError;
    use gpt2apirs_generate::text::StreamSnapshot;

    /// 源流未结束时就必须已经送出 delta。
    #[tokio::test]
    async fn delta_arrives_before_source_closes() {
        let (sender, handle) = channel_handle();
        let response = build_live_stream(handle, gpt2apirs_generate::models::DEFAULT_TEXT_MODEL);
        let mut body = response.into_body().into_data_stream();

        sender.send(Ok(gpt2apirs_protocol::text_stream::Delta::Text("第一段".to_string()))).await.expect("发送");

        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            let mut buffer = String::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.expect("响应块");
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                if buffer.contains("content_block_delta") && buffer.contains("第一段") {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(seen, Ok(true), "delta 必须在源流结束前送出");
    }

    /// Anthropic 事件顺序：message_start → content_block_start → delta →
    /// content_block_stop → message_delta → message_stop。
    #[tokio::test]
    async fn events_are_ordered() {
        let (sender, handle) = channel_handle();
        let snapshot = handle.snapshot.clone();
        if let Ok(mut guard) = snapshot.lock() {
            *guard = StreamSnapshot {
                reasoning: String::new(),
                text: "你好世界".to_string(),
                conversation_id: "conv".to_string(),
                finished: true,
            };
        }
        let response = build_live_stream(handle, gpt2apirs_generate::models::DEFAULT_TEXT_MODEL);

        sender.send(Ok(gpt2apirs_protocol::text_stream::Delta::Text("你好".to_string()))).await.expect("发送");
        sender.send(Ok(gpt2apirs_protocol::text_stream::Delta::Text("世界".to_string()))).await.expect("发送");
        drop(sender);

        let body = collect_body(response).await;
        assert_before(&body, "event: message_start", "event: content_block_start");
        assert_before(&body, "event: content_block_start", "event: content_block_delta");
        assert_before(&body, "event: content_block_delta", "event: content_block_stop");
        assert_before(&body, "event: content_block_stop", "event: message_delta");
        assert_before(&body, "event: message_delta", "event: message_stop");
        assert!(body.contains("\"stop_reason\":\"end_turn\""), "应给出结束原因");
    }

    /// 上游出错：部分结果保留，error 事件出现在收尾之前。
    #[tokio::test]
    async fn upstream_error_is_reported_at_tail() {
        let (sender, handle) = channel_handle();
        let response = build_live_stream(handle, gpt2apirs_generate::models::DEFAULT_TEXT_MODEL);

        sender.send(Ok(gpt2apirs_protocol::text_stream::Delta::Text("部分".to_string()))).await.expect("发送");
        sender
            .send(Err(GenerateError::new(
                gpt2apirs_generate::error::FailureKind::Network,
                "连接重置",
            )))
            .await
            .expect("发送");
        drop(sender);

        let body = collect_body(response).await;
        assert!(body.contains("部分"), "已生成的部分不能丢");
        assert!(body.contains("连接重置"), "错误原因应报给客户端");
        assert_before(&body, "event: content_block_stop", "event: error");
        assert!(body.contains("event: message_stop"), "出错也要有终止事件");
    }
}

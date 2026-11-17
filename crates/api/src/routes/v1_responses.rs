//! `POST /v1/responses` —— OpenAI Responses API（文本）。

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

/// `POST /v1/responses` 的请求体。
#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    /// 模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// 输入：字符串，或 `[{role, content}]`，或 `[{type:"input_text",text}]`。
    pub input: Value,
    /// 系统指令（等价于 system 消息）。
    #[serde(default)]
    pub instructions: Option<String>,
    /// 是否流式。
    #[serde(default)]
    pub stream: Option<bool>,
    /// 工具列表（**不支持**，出现即拒）。
    #[serde(default)]
    pub tools: Option<Value>,
    /// `max_output_tokens`：接受但忽略。
    #[serde(default)]
    pub max_output_tokens: Option<i64>,
}

fn part_text(part: &Value) -> Option<String> {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    None
}

fn is_content_part(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("input_text" | "output_text" | "text")
    )
}

/// 把 `input` 归一成 `(role, text)` 列表。
fn messages_from_input(input: &Value, instructions: Option<&str>) -> Result<Vec<(String, String)>, ApiError> {
    let mut messages: Vec<(String, String)> = Vec::new();
    if let Some(system) = instructions.map(str::trim).filter(|text| !text.is_empty()) {
        messages.push(("system".to_string(), system.to_string()));
    }

    let push = |role: String, text: String, messages: &mut Vec<(String, String)>| {
        if !text.trim().is_empty() {
            messages.push((role, text));
        }
    };

    match input {
        Value::String(text) => push("user".to_string(), text.clone(), &mut messages),
        Value::Object(_) => {
            let role = input
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_string();
            let text = collect_content(input.get("content").unwrap_or(&Value::Null))?;
            push(role, text, &mut messages);
        }
        Value::Array(items) => {
            // 全是内容片段  视作一条 user 消息
            if !items.is_empty() && items.iter().all(is_content_part) {
                let mut text = String::new();
                for item in items {
                    if let Some(part) = part_text(item) {
                        text.push_str(&part);
                    }
                }
                push("user".to_string(), text, &mut messages);
            } else {
                for item in items {
                    if let Some(role) = item.get("role").and_then(Value::as_str) {
                        let text = collect_content(item.get("content").unwrap_or(&Value::Null))?;
                        push(role.to_string(), text, &mut messages);
                    } else if is_content_part(item) {
                        if let Some(part) = part_text(item) {
                            push("user".to_string(), part, &mut messages);
                        }
                    }
                }
            }
        }
        Value::Null => {}
        other => {
            return Err(ApiError::openai(gpt2apirs_core::AppError::invalid_param(
                format!("input 类型不支持：{other}"),
                "input",
            )));
        }
    }
    Ok(messages)
}

fn collect_content(content: &Value) -> Result<String, ApiError> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("input_text");
                match kind {
                    "input_text" | "output_text" | "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    "input_image" | "image_url" | "image" => {
                        return Err(ApiError::openai(
                            gpt2apirs_core::AppError::invalid_param(
                                "文本端点不支持图片输入；请用 /v1/images/edits",
                                "input",
                            ),
                        ));
                    }
                    _ => {}
                }
            }
            Ok(out)
        }
        _ => Ok(String::new()),
    }
}

fn usage_payload(prompt: i64, completion: i64) -> Value {
    json!({
        "input_tokens": prompt,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": completion,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": prompt + completion,
    })
}

fn output_item(item_id: &str, text: &str, status: &str) -> Value {
    json!({
        "id": item_id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

fn response_envelope(
    response_id: &str,
    model: &str,
    created: i64,
    status: &str,
    output: Value,
    usage: Option<Value>,
) -> Value {
    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": created,
        "status": status,
        "error": null,
        "incomplete_details": null,
        "model": model,
        "output": output,
        "parallel_tool_calls": false,
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    response
}

/// `POST /v1/responses`
pub async fn create_response(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResponsesRequest>,
) -> Result<Response, ApiError> {
    if crate::routes::text_common::has_tools(&request.tools) {
        return Err(ApiError::openai(
            crate::routes::text_common::tools_not_supported("responses.tools"),
        ));
    }

    let messages = messages_from_input(&request.input, request.instructions.as_deref())?;
    if messages.iter().all(|(_, text)| text.trim().is_empty()) {
        return Err(ApiError::openai(gpt2apirs_core::AppError::invalid_param(
            "input 里没有非空文本",
            "input",
        )));
    }

    let model = resolve_model(request.model.as_deref());
    let deadline = default_request_deadline(&state.settings);

    // 真流式：命中时直接返回 SSE，不走「先跑完再切块」
    if request.stream.unwrap_or(false) {
        let handle = start_text_stream(&state, messages.clone(), Vec::new(), &model, deadline).await?;
        return Ok(build_live_stream(handle, &model).into_response());
    }

    let outcome = run_text(&state, &messages, &[], &model, deadline).await?;

    let prompt_tokens = gpt2apirs_core::tokens::count_message_text_tokens(&messages, &model);
    let completion_tokens = gpt2apirs_core::tokens::count_text_tokens(&outcome.result.text, &model);
    let usage = usage_payload(prompt_tokens, completion_tokens);
    let created = chrono::Utc::now().timestamp();
    let response_id = format!("resp_{}", Uuid::new_v4().simple());
    let item_id = format!("msg_{}", Uuid::new_v4().simple());

    tracing::info!(
        account = %outcome.account_id,
        model = %model,
        conversation_id = %outcome.result.conversation_id,
        input_tokens = prompt_tokens,
        output_tokens = completion_tokens,
        sse_bytes = outcome.diag.sse_bytes,
        "responses 生成完成"
    );

    Ok(Json(response_envelope(
        &response_id,
        &model,
        created,
        "completed",
        json!([output_item(&item_id, &outcome.result.text, "completed")]),
        Some(usage),
    ))
    .into_response())
}

/// 把流式 handle 变成 axum SSE 响应（Responses 事件格式）。
///
/// 事件顺序：`response.created` → `response.output_item.added` →
/// 若干 `response.output_text.delta` → `response.output_text.done` →
/// `response.output_item.done` →（可选 `error`）→ `response.completed`。
fn build_live_stream(handle: StreamHandle, model: &str) -> axum::response::Response {
    use axum::response::sse::Sse;
    use futures_util::StreamExt;
    use std::convert::Infallible;

    let response_id = format!("resp_{}", Uuid::new_v4().simple());
    let item_id = format!("msg_{}", Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model = model.to_string();
    let snapshot = handle.snapshot.clone();
    let messages = handle.billing_messages.clone();
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // 1) 开头两件
    let head = futures_util::stream::iter(vec![
        Ok::<_, Infallible>(sse_event(
            "response.created",
            json!({
                "response": response_envelope(
                    &response_id, &model, created, "in_progress", json!([]), None,
                ),
            }),
        )),
        Ok::<_, Infallible>(sse_event(
            "response.output_item.added",
            json!({ "output_index": 0, "item": output_item(&item_id, "", "in_progress") }),
        )),
    ]);

    // 2) 正文增量：来一段转一段
    let body_item_id = item_id.clone();
    let body_errors = last_error.clone();
    let body = handle.deltas.filter_map(move |item| {
        let id = body_item_id.clone();
        let errors = body_errors.clone();
        async move {
            match item {
                Ok(delta) => Some(Ok::<_, Infallible>(sse_event(
                    "response.output_text.delta",
                    json!({
                        "item_id": id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": delta,
                    }),
                ))),
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

    // 3) 收尾：用量只有全文出来后才能算
    let tail_response_id = response_id.clone();
    let tail_item_id = item_id.clone();
    let tail_model = model.clone();
    let tail_snapshot = snapshot;
    let tail_messages = messages;
    let tail_errors = last_error;
    let tail = futures_util::stream::once(async move {
        let text = tail_snapshot
            .lock()
            .map(|guard| guard.text.clone())
            .unwrap_or_default();
        let (prompt, completion) =
            crate::routes::text_common::token_counts(&tail_messages, &text, &tail_model);

        let item = output_item(&tail_item_id, &text, "completed");
        let mut events = Vec::with_capacity(4);
        events.push(sse_event(
            "response.output_text.done",
            json!({
                "item_id": tail_item_id,
                "output_index": 0,
                "content_index": 0,
                "text": text,
            }),
        ));
        events.push(sse_event(
            "response.output_item.done",
            json!({ "output_index": 0, "item": item.clone() }),
        ));
        if let Some(error) = tail_errors.lock().ok().and_then(|guard| guard.clone()) {
            events.push(sse_event(
                "error",
                json!({ "message": error, "type": "upstream_error" }),
            ));
        }
        events.push(sse_event(
            "response.completed",
            json!({
                "response": response_envelope(
                    &tail_response_id,
                    &tail_model,
                    created,
                    "completed",
                    json!([item]),
                    Some(usage_payload(prompt, completion)),
                ),
            }),
        ));
        futures_util::stream::iter(events.into_iter().map(Ok::<_, Infallible>))
    })
    .flatten();

    Sse::new(head.chain(body).chain(tail)).into_response()
}

/// 组一条 SSE 事件：`data` 里带 `type` 字段（与官方事件体一致）。
fn sse_event(kind: &str, mut payload: Value) -> axum::response::sse::Event {
    if let Value::Object(map) = &mut payload {
        map.insert("type".to_string(), json!(kind));
    }
    axum::response::sse::Event::default().data(payload.to_string())
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

        sender.send(Ok("第一段".to_string())).await.expect("发送");

        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            let mut buffer = String::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.expect("响应块");
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                if buffer.contains("response.output_text.delta") && buffer.contains("第一段") {
                    return true;
                }
            }
            false
        })
        .await;

        // sender 仍然存活 —— 源流尚未结束，此处若为缓冲实现会一直等到超时
        assert_eq!(seen, Ok(true), "delta 必须在源流结束前送出");
    }

    /// 事件顺序固定，且 done 事件里是全文。
    #[tokio::test]
    async fn events_are_ordered_and_done_carries_full_text() {
        let (sender, handle) = channel_handle();
        let snapshot = handle.snapshot.clone();
        if let Ok(mut guard) = snapshot.lock() {
            *guard = StreamSnapshot {
                text: "你好世界".to_string(),
                conversation_id: "conv".to_string(),
                finished: true,
            };
        }
        let response = build_live_stream(handle, gpt2apirs_generate::models::DEFAULT_TEXT_MODEL);

        sender.send(Ok("你好".to_string())).await.expect("发送");
        sender.send(Ok("世界".to_string())).await.expect("发送");
        drop(sender);

        let body = collect_body(response).await;
        assert_before(&body, "response.created", "response.output_item.added");
        assert_before(&body, "response.output_item.added", "response.output_text.delta");
        assert_before(&body, "response.output_text.delta", "response.output_text.done");
        assert_before(&body, "response.output_text.done", "response.output_item.done");
        assert_before(&body, "response.output_item.done", "response.completed");
        assert!(body.contains("\"text\":\"你好世界\""), "done 事件应带全文");
        assert!(body.contains("\"status\":\"completed\""), "收尾状态应为 completed");
    }

    /// 上游出错时：已生成的部分照常送出，末尾补 error 事件并正常收尾。
    #[tokio::test]
    async fn upstream_error_is_reported_at_tail() {
        let (sender, handle) = channel_handle();
        let response = build_live_stream(handle, gpt2apirs_generate::models::DEFAULT_TEXT_MODEL);

        sender.send(Ok("部分".to_string())).await.expect("发送");
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
        assert_before(&body, "response.output_text.done", "\"type\":\"error\"");
        assert!(body.contains("response.completed"), "出错也要有收尾事件");
    }
}

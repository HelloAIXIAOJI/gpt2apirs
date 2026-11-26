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
    /// 角色：`system` / `user` / `assistant` / `tool`。
    pub role: String,
    /// 内容：字符串，或 `[{"type":"text","text":"..."}]`。
    pub content: Value,
    /// `role: tool` 时表示这是哪个调用的结果（编码进正文，上游不认字段）。
    #[serde(default)]
    pub tool_call_id: Option<String>,
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
    /// 流式选项：`{"include_usage": true}` 时按 OpenAI 的格式单独发一个 usage 块。
    #[serde(default)]
    pub stream_options: Option<Value>,
    /// 工具列表。只有 `model: text_tool` 接受；`text` 收到就报错。
    #[serde(default)]
    pub tools: Option<Value>,
    /// 工具选择策略：`auto` / `required`，或指定某个函数（要求必须调）。
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// 输出模态（只支持 `text`；要图片请走生图端点）。
    #[serde(default)]
    pub modalities: Option<Value>,
    /// 思考强度（OpenAI 侧叫法）。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 思考强度（部分客户端用这个叫法）。
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// `reasoning: { effort: "low" }` 这种嵌套写法。
    #[serde(default)]
    pub reasoning: Option<Value>,
}

/// 客户端是否要求「单独的 usage 块」。
///
/// OpenAI 官方的流式把用量放在**最后一个 `choices: []` 的空块**里，
/// 而且要显式 `stream_options: {include_usage: true}` 才发。
/// 我们默认是塞进 finish 块（多给不会让标准客户端出错），
/// 但严格按规范写的客户端只认那个空块 —— 它要就照它的来。
fn wants_usage_chunk(options: &Option<Value>) -> bool {
    options
        .as_ref()
        .and_then(|value| value.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 从三种写法里取思考强度，原样透传给上游。
///
/// 优先级：`thinking_effort` > `reasoning_effort` > `reasoning.effort`。
/// 取到就原样发，不做归一化 —— 上游认不认由它决定，我们别替它猜。
fn resolve_thinking_effort(request: &ChatCompletionRequest) -> Option<String> {
    let from_nested = request
        .reasoning
        .as_ref()
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    request
        .thinking_effort
        .clone()
        .or_else(|| request.reasoning_effort.clone())
        .or(from_nested)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 校验 `modalities`：只接受纯文本输出。
///
/// 这是**声明式能力开关**，不能像 temperature 那样默默忽略 ——
/// 客户端说「我要图片模态」而我们只回文字，等于让它在错误的假设上继续跑。
fn ensure_text_modalities(modalities: &Option<Value>) -> Result<(), ApiError> {
    let Some(value) = modalities else { return Ok(()) };
    let items = match value {
        Value::Null => return Ok(()),
        Value::Array(items) => items,
        other => {
            return Err(ApiError::openai(AppError::invalid_param(
                format!("modalities 必须是字符串数组，收到：{other}"),
                "modalities",
            )));
        }
    };
    for item in items {
        let Some(kind) = item.as_str() else {
            return Err(ApiError::openai(AppError::invalid_param(
                format!("modalities 的元素必须是字符串，收到：{item}"),
                "modalities",
            )));
        };
        if kind.trim().eq_ignore_ascii_case("text") {
            continue;
        }
        return Err(ApiError::openai(AppError::invalid_param(
            format!(
                "不支持输出模态 {kind:?}；本实现只返回文本，要出图请用 POST /v1/images/generations"
            ),
            "modalities",
        )));
    }
    Ok(())
}

/// 一条消息的解析结果：纯文本 + 待上传的图片来源。
struct ChatContent {
    text: String,
    images: Vec<gpt2apirs_generate::inputs::ImageSource>,
}

/// 把一条消息的 `content` 拆成纯文本与图片。
///
/// 图片在这里只做**收集**，上传要等拿到账号 token 之后（换号要重传）。
fn message_content(message: &ChatMessage) -> Result<ChatContent, ApiError> {
    match &message.content {
        Value::Null => Ok(ChatContent { text: String::new(), images: Vec::new() }),
        Value::String(text) => Ok(ChatContent { text: text.clone(), images: Vec::new() }),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" | "input_text" => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    "image_url" | "image" | "input_image" => {
                        images.push(image_source_from_part(part)?);
                    }
                    _ => {}
                }
            }
            Ok(ChatContent { text, images })
        }
        other => Err(ApiError::openai(AppError::invalid_param(
            format!("content 类型不支持：{other}"),
            "messages",
        ))),
    }
}

/// 从一个图片 part 里取出图片来源。
///
/// 客户端写法不统一：`image_url` 可能是字符串，也可能是 `{url}` 对象；
/// 有的直接给 `data`。这里按「对象里找 url / 字符串本身就是 url」处理。
fn image_source_from_part(
    part: &Value,
) -> Result<gpt2apirs_generate::inputs::ImageSource, ApiError> {
    let candidate = part
        .get("image_url")
        .and_then(|value| value.get("url").or(Some(value)))
        .and_then(Value::as_str)
        .or_else(|| part.get("url").and_then(Value::as_str))
        .or_else(|| part.get("data").and_then(Value::as_str));
    let Some(raw) = candidate else {
        return Err(ApiError::openai(AppError::invalid_param(
            "图片 part 缺少 url / data",
            "messages",
        )));
    };
    image_source_from_string(raw)
}

/// 把一段字符串转成图片来源：data URL / http(s) URL / 裸 base64。
fn image_source_from_string(
    raw: &str,
) -> Result<gpt2apirs_generate::inputs::ImageSource, ApiError> {
    use gpt2apirs_generate::inputs::ImageSource;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "图片地址为空",
            "messages",
        )));
    }
    if trimmed.starts_with("data:") {
        return Ok(ImageSource::DataUrl(trimmed.to_string()));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(ImageSource::RemoteUrl(trimmed.to_string()));
    }
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|_| {
            ApiError::openai(AppError::invalid_param(
                "图片既不是 URL 也不是合法 base64",
                "messages",
            ))
        })?;
    Ok(ImageSource::Inline {
        data,
        filename: String::new(),
        mime: String::new(),
    })
}

fn normalize_role(role: &str) -> Result<String, ApiError> {
    let normalized = role.trim().to_ascii_lowercase();
    match normalized.as_str() {
        // `tool` 的结果要回填给模型：上游没有工具概念，只能当成一条 user 说明。
        // 不转成 user 就没法多轮——客户端第二轮必然发 role=tool。
        "tool" => Ok("user".to_string()),
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
    ensure_text_modalities(&request.modalities)?;
    if request.messages.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "messages 不能为空",
            "messages",
        )));
    }

    // ---- 归一化 ----
    let mut messages: Vec<(String, String)> = Vec::with_capacity(request.messages.len());
    let mut images = Vec::new();
    for message in &request.messages {
        let role = normalize_role(&message.role)?;
        let parsed = message_content(message)?;
        let is_tool_result = message.role.trim().eq_ignore_ascii_case("tool");
        let text = if is_tool_result {
            // 带上 call_id，模型才知道这是哪次调用的结果
            let id = message.tool_call_id.as_deref().unwrap_or("").trim();
            if id.is_empty() {
                format!("Tool result:\n{}", parsed.text)
            } else {
                format!("Tool result ({id}):\n{}", parsed.text)
            }
        } else {
            parsed.text
        };
        messages.push((role, text));
        images.extend(parsed.images);
    }
    let has_content = messages.iter().any(|(_, text)| !text.trim().is_empty());
    if !has_content && images.is_empty() {
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
        .unwrap_or_else(|| gpt2apirs_generate::models::DEFAULT_TEXT_MODEL.to_string());
    let deadline = gpt2apirs_generate::default_request_deadline(&state.settings);

    // ---- 工具调用（只有 text_tool 允许）----
    // `text` 收到 tools 必须报错而不是静默忽略：客户端会一直等一个永远不来的
    // tool_calls，最后要么超时，要么把普通文本当成工具结果解析。
    let tools = crate::tool_call::parse_tools(
        request.tools.as_ref().unwrap_or(&Value::Null),
    );
    if !tools.is_empty() && !gpt2apirs_generate::models::is_tool_model(&requested_model) {
        return Err(ApiError::openai(AppError::InvalidRequest {
            message: format!(
                "模型 {requested_model} 不支持工具调用；要用请把 model 改成 {}",
                gpt2apirs_generate::models::EXPOSED_TEXT_TOOL_MODEL
            ),
            param: Some("tools".to_string()),
            code: Some("tools_not_supported".to_string()),
        }));
    }
    let tool_prompt = if !tools.is_empty() {
        Some(crate::tool_call::build_tool_prompt(
            &tools,
            crate::tool_call::requires_tool_call(
                request.tool_choice.as_ref().unwrap_or(&Value::Null),
            ),
        ))
    } else {
        None
    };

    let effort = resolve_thinking_effort(&request);
    let include_usage = wants_usage_chunk(&request.stream_options);

    if request.stream.unwrap_or(false) {
        // ---- 真流式：上游每吐一段就转发一段 ----
        let handle = start_text_stream(
            &state,
            messages.clone(),
            images.clone(),
            &requested_model,
            deadline,
            effort.as_deref(),
            tool_prompt.as_deref(),
        )
        .await?;
        // 带工具时不走「边收边吐」：工具调用要等全文才能解析，
        // 提前吐出的正文里可能混着 <tool_calls> 标签，客户端会看到裸标签。
        if tool_prompt.is_some() {
            return Ok(
                build_tool_stream(handle, &requested_model, include_usage).into_response(),
            );
        }
        return Ok(build_live_stream(handle, &requested_model, include_usage).into_response());
    }

    // ---- 取号  生成  记账（与其他文本端点共用）----
    let outcome = run_text(
        &state,
        &messages,
        &images,
        &requested_model,
        deadline,
        effort.as_deref(),
        tool_prompt.as_deref(),
    )
    .await?;
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

    // ---- 工具调用：解析失败就当普通文本（绝不因为「没按格式写」而报错）----
    let (clean_text, calls) = if tool_prompt.is_some() {
        crate::tool_call::parse_tool_calls(&result.text)
    } else {
        (result.text.clone(), Vec::new())
    };
    let mut message = if calls.is_empty() {
        json!({ "role": "assistant", "content": clean_text })
    } else {
        // 有工具调用时正文可能为空：OpenAI 允许 content 为 null
        let content = if clean_text.trim().is_empty() {
            Value::Null
        } else {
            json!(clean_text)
        };
        json!({
            "role": "assistant",
            "content": content,
            "tool_calls": crate::tool_call::openai_tool_calls(&calls),
        })
    };
    if !result.reasoning.trim().is_empty() {
        // 思考内容单列一个字段：客户端想看就显示，不看也不会混进正文
        message["reasoning_content"] = json!(result.reasoning);
    }
    let finish_reason = if calls.is_empty() { "stop" } else { "tool_calls" };

    Ok(Json(json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": created,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
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

fn build_live_stream(handle: StreamHandle, model: &str, include_usage: bool) -> axum::response::Response {
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
                Ok(delta) => {
                    // 思考内容单独放 `reasoning_content`，正文永远只有 `content`
                    let payload = match delta {
                        gpt2apirs_protocol::text_stream::Delta::Text(text) => {
                            json!({ "content": text })
                        }
                        gpt2apirs_protocol::text_stream::Delta::Reasoning(text) => {
                            json!({ "reasoning_content": text })
                        }
                    };
                    Some(Ok(Event::default().data(chunk_payload(
                        &id,
                        &model,
                        created,
                        payload,
                        Value::Null,
                        None,
                    ))))
                }
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
        // 要了独立 usage 块就不在 finish 块里重复给：官方实现就是这么分的，
        // 两边都给会让按规范解析的客户端重复记账。
        let finish_usage = if include_usage {
            None
        } else {
            Some(usage.clone())
        };
        let mut payload: Value = serde_json::from_str(&chunk_payload(
            &tail_id,
            &tail_model,
            created,
            json!({}),
            json!("stop"),
            finish_usage,
        ))
        .unwrap_or_else(|_| json!({}));
        if let Some(error) = tail_errors.lock().ok().and_then(|guard| guard.clone()) {
            // OpenAI 的流里没有「错误块」的标准位置，这里用扩展字段说明
            payload["error"] = json!({ "message": error, "type": "upstream_error" });
        }
        let mut events = vec![Event::default().data(payload.to_string())];
        if include_usage {
            // 官方格式：`choices` 为空的独立块，只放 usage
            events.push(Event::default().data(
                usage_chunk_payload(&tail_id, &tail_model, created, usage),
            ));
        }
        futures_util::stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>))
    })
    .flatten();

    // 4) [DONE]
    let done = futures_util::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
    });

    Sse::new(head.chain(body).chain(tail).chain(done))
        .keep_alive(super::sse_keep_alive())
        .into_response()
}

/// 带工具时的流式：等全文生成完再解析，再按 OpenAI 的流格式吐出去。
///
/// 为什么不边收边吐：工具调用要看到完整输出才能解析；提前把正文发出去，
/// 客户端会收到 `<tool_calls>` 裸标签。代价是首字延迟等于总生成时间，
/// 这是模拟工具调用的必然取舍。
fn build_tool_stream(handle: StreamHandle, model: &str, include_usage: bool) -> axum::response::Response {
    use axum::response::sse::{Event, Sse};
    use futures_util::StreamExt;

    let completion_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model = model.to_string();
    let snapshot = handle.snapshot.clone();
    let billing = handle.billing_messages.clone();

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

    // 增量必须被消费：槽位与记账挂在流上，不驱动就等于请求没结束；正文不往外发
    let drain = handle
        .deltas
        .filter_map(|_| async { None::<Result<Event, std::convert::Infallible>> });

    let tail_id = completion_id.clone();
    let tail_model = model.clone();
    let tail = futures_util::stream::once(async move {
        let text = snapshot
            .lock()
            .map(|guard| guard.text.clone())
            .unwrap_or_default();
        let (clean, calls) = crate::tool_call::parse_tool_calls(&text);
        let usage = build_usage(&billing, &text, &tail_model);
        let mut events: Vec<Result<Event, std::convert::Infallible>> = Vec::new();
        if !clean.trim().is_empty() {
            events.push(Ok(Event::default().data(chunk_payload(
                &tail_id,
                &tail_model,
                created,
                json!({ "content": clean }),
                Value::Null,
                None,
            ))));
        }
        if !calls.is_empty() {
            // arguments 一次发完整（OpenAI 允许分片，一次发也合法）
            let deltas: Vec<Value> = calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    json!({
                        "index": index,
                        "id": format!("call_{}", Uuid::new_v4().simple()),
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments_json() },
                    })
                })
                .collect();
            events.push(Ok(Event::default().data(chunk_payload(
                &tail_id,
                &tail_model,
                created,
                json!({ "tool_calls": deltas }),
                Value::Null,
                None,
            ))));
        }
        let finish = if calls.is_empty() { "stop" } else { "tool_calls" };
        let finish_usage = if include_usage {
            None
        } else {
            Some(usage.clone())
        };
        events.push(Ok(Event::default().data(chunk_payload(
            &tail_id,
            &tail_model,
            created,
            json!({}),
            json!(finish),
            finish_usage,
        ))));
        if include_usage {
            events.push(Ok(Event::default().data(usage_chunk_payload(
                &tail_id, &tail_model, created, usage,
            ))));
        }
        futures_util::stream::iter(events)
    })
    .flatten();

    let done = futures_util::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
    });

    Sse::new(head.chain(drain).chain(tail).chain(done))
        .keep_alive(super::sse_keep_alive())
        .into_response()
}

/// 独立的 usage 块：`choices` 为空数组，只有 `usage`（OpenAI 官方的流式格式）。
fn usage_chunk_payload(completion_id: &str, model: &str, created: i64, usage: Value) -> String {
    json!({
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
        "usage": usage,
    })
    .to_string()
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

#[cfg(test)]
mod keepalive_tests {
    use super::*;
    use std::time::Duration;

    use futures_util::StreamExt;

    /// 思考期没有可见文本时，必须每隔两秒发一次心跳注释帧。
    ///
    /// 线上表现：上游思考约 10 秒，我们一个字节都不发 → 客户端当成卡死，
    /// 中间的代理还会攒包，首字节被推到十几秒。
    #[tokio::test]
    async fn idle_stream_emits_keep_alive_comments() {
        let (sender, handle) = crate::routes::test_support::channel_handle();
        let mut body = build_live_stream(handle, "text", false).into_body().into_data_stream();

        // 第一块是 role 块，立刻就得到
        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("role 块应立刻到达")
            .expect("应有数据")
            .expect("读块失败");
        let first = String::from_utf8_lossy(&first).to_string();
        assert!(first.contains("\"role\":\"assistant\""), "首块应是 role：{first}");

        // 之后不喂任何 delta：2 秒后应收到心跳
        let next = tokio::time::timeout(Duration::from_secs(4), body.next())
            .await
            .expect("空闲 2 秒后应有心跳")
            .expect("应有数据")
            .expect("读块失败");
        let next = String::from_utf8_lossy(&next).to_string();
        assert!(next.contains(": keepalive"), "应是心跳注释帧：{next}");

        drop(sender);
    }

    /// 有真实增量时不该插心跳：正文旁边只允许出现自己的事件。
    #[tokio::test]
    async fn streaming_deltas_do_not_trigger_keep_alive() {
        let (sender, handle) = crate::routes::test_support::channel_handle();
        sender.send(Ok(gpt2apirs_protocol::text_stream::Delta::Text("第一段".to_string()))).await.ok();
        let mut body = build_live_stream(handle, "text", false).into_body().into_data_stream();

        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("role 块")
            .expect("应有数据")
            .expect("读块失败");
        assert!(String::from_utf8_lossy(&first).contains("assistant"));

        let second = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("正文块")
            .expect("应有数据")
            .expect("读块失败");
        let second = String::from_utf8_lossy(&second).to_string();
        assert!(second.contains("第一段"), "第二块应是正文：{second}");
        assert!(!second.contains("keepalive"), "有内容时不该插心跳：{second}");

        drop(sender);
    }
}
#[cfg(test)]
mod reasoning_tests {
    use super::*;
    use std::time::Duration;

    use futures_util::StreamExt;

    /// 思考内容必须走 `reasoning_content`，不能混进 `content`。
    #[tokio::test]
    async fn reasoning_delta_is_sent_on_its_own_field() {
        let (sender, handle) = crate::routes::test_support::channel_handle();
        sender
            .send(Ok(gpt2apirs_protocol::text_stream::Delta::Reasoning(
                "先想一想".to_string(),
            )))
            .await
            .ok();
        sender
            .send(Ok(gpt2apirs_protocol::text_stream::Delta::Text(
                "答案".to_string(),
            )))
            .await
            .ok();

        let mut body = build_live_stream(handle, "text", false).into_body().into_data_stream();

        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("role 块")
            .expect("应有数据")
            .expect("读块失败");
        assert!(String::from_utf8_lossy(&first).contains("assistant"));

        let second = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("思考块")
            .expect("应有数据")
            .expect("读块失败");
        let second = String::from_utf8_lossy(&second).to_string();
        assert!(second.contains("reasoning_content"), "思考内容要单列：{second}");
        assert!(second.contains("先想一想"), "{second}");
        assert!(!second.contains("\"content\":\"先想一想\""), "不能混进正文：{second}");

        let third = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("正文块")
            .expect("应有数据")
            .expect("读块失败");
        let third = String::from_utf8_lossy(&third).to_string();
        assert!(third.contains("\"content\":\"答案\""), "正文走 content：{third}");
        assert!(!third.contains("reasoning_content"), "{third}");

        drop(sender);
    }

    /// 三种写法都能取到思考强度，且原样保留。
    #[test]
    fn thinking_effort_is_resolved_from_three_shapes() {
        let base = |json: &str| -> ChatCompletionRequest {
            serde_json::from_str(json).expect("请求体应可解析")
        };

        assert_eq!(
            resolve_thinking_effort(&base(
                r#"{"messages":[{"role":"user","content":"hi"}],"thinking_effort":"low"}"#
            )),
            Some("low".to_string())
        );
        assert_eq!(
            resolve_thinking_effort(&base(
                r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"xhigh"}"#
            )),
            Some("xhigh".to_string())
        );
        assert_eq!(
            resolve_thinking_effort(&base(
                r#"{"messages":[{"role":"user","content":"hi"}],"reasoning":{"effort":"medium"}}"#
            )),
            Some("medium".to_string())
        );
        // 都没给 = 不传
        assert_eq!(
            resolve_thinking_effort(&base(r#"{"messages":[{"role":"user","content":"hi"}]}"#)),
            None
        );
    }

    #[test]
    fn include_usage_flag_is_read_from_stream_options() {
        let parse = |json: &str| -> ChatCompletionRequest {
            serde_json::from_str(json).expect("请求体应可解析")
        };
        let on = parse(
            r#"{"messages":[{"role":"user","content":"hi"}],"stream_options":{"include_usage":true}}"#,
        );
        assert!(wants_usage_chunk(&on.stream_options));
        let off = parse(
            r#"{"messages":[{"role":"user","content":"hi"}],"stream_options":{"include_usage":false}}"#,
        );
        assert!(!wants_usage_chunk(&off.stream_options));
        // 不传 = 老行为：usage 塞在 finish 块里
        let none = parse(r#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert!(!wants_usage_chunk(&none.stream_options));
    }

    /// 要了独立 usage 块：finish 块不再带 usage，后面单独来一个 `choices: []` 的。
    #[tokio::test]
    async fn usage_chunk_is_sent_when_requested() {
        let (sender, handle) = crate::routes::test_support::channel_handle();
        sender
            .send(Ok(gpt2apirs_protocol::text_stream::Delta::Text(
                "答案".to_string(),
            )))
            .await
            .ok();
        drop(sender);

        let mut body = build_live_stream(handle, "text", true)
            .into_body()
            .into_data_stream();
        let mut chunks: Vec<String> = Vec::new();
        // next() 给的是 Result<Bytes, _>，得剥两层
        while let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_secs(1), body.next()).await
        {
            chunks.push(String::from_utf8_lossy(&chunk).to_string());
        }
        let text = chunks.join("");
        let finish = chunks
            .iter()
            .find(|c| c.contains("finish_reason\":\"stop"))
            .expect("应有 finish 块");
        assert!(!finish.contains("\"usage\""), "要了独立块就别在 finish 里重复给：{finish}");
        let usage = chunks
            .iter()
            .find(|c| c.contains("\"usage\""))
            .expect("应有独立 usage 块");
        assert!(usage.contains("\"choices\":[]"), "usage 块的 choices 必须是空数组：{usage}");
        assert!(usage.contains("total_tokens"), "{usage}");
        assert!(text.contains("[DONE]"), "usage 块要在 [DONE] 之前");
    }
}

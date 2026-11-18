//! 文本生成链路。
//!
//! 与图片链路的差异只在**请求构造**，握手（bootstrap + sentinel）完全共用：
//!
//! 与生图的差异：不做 `conversation/prepare`；`system_hints` 传空；输出靠增量补丁而不是 sediment id。
//! bootstrap 与 sentinel 两条链路共用同一个 `handshake`。

use anyhow::Result;
use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::conversation::{
    build_text_conversation_payload, build_text_conversation_payload_with_images,
};
use gpt2apirs_protocol::endpoints::{CONVERSATION, url as upstream_url};
use gpt2apirs_protocol::text_stream::{TextAccumulator, collect_text};

use crate::error::{FailureKind, GenerateError, classify_http};
use crate::{
    DEFAULT_USER_AGENT, FlowDiagnostics, Handshake, handshake, image_headers, stage_budget,
    stage_caps,
};

/// 文本生成请求。
#[derive(Debug, Clone)]
pub struct TextRequest {
    /// 用户提示词。
    pub prompt: String,
    /// 对外模型名。
    pub model: String,
    /// 上游模型 slug。
    pub upstream_model: String,
    /// User-Agent。
    pub user_agent: String,
    /// 会被上游回放的历史助手正文（由 [`TextRequest::with_messages`] 自动派生）。
    history: Vec<String>,
    /// 多轮消息（非空时优先于 `prompt`）。
    pub messages: Vec<(String, String)>,
    /// 已上传到上游的参考图（挂在最后一条 user 消息上）。
    pub images: Vec<gpt2apirs_protocol::conversation::ImageReference>,
}

impl TextRequest {
    /// 用提示词构造默认请求。
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: "gpt-5-5".to_string(),
            upstream_model: "gpt-5-5".to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            history: Vec::new(),
            messages: Vec::new(),
            images: Vec::new(),
        }
    }

    /// 带上多轮消息（chat/completions 用）。
    ///
    /// 历史回放判定所需的助手正文在这里**自动派生**：
    /// 调用方只负责给消息，不必记得再填一份 history（漏填就是线上重复回复那个 bug）。
    pub fn with_messages(mut self, messages: Vec<(String, String)>) -> Self {
        self.history = gpt2apirs_protocol::text_stream::assistant_history(&messages);
        self.messages = messages;
        self
    }

    /// 会被上游回放的历史助手正文。
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// 带上参考图（图文对话用）。
    pub fn with_images(
        mut self,
        images: Vec<gpt2apirs_protocol::conversation::ImageReference>,
    ) -> Self {
        self.images = images;
        self
    }
}

/// 文本生成结果。
#[derive(Debug, Clone, Default)]
pub struct TextResult {
    /// 完整回复文本（已清洗）。
    /// 
    /// 与图片不同：这里必须**逐字保留**用户可见内容，
    /// 任何缩写、补全、格式化都会让下游误以为模型真的那样说。
    pub text: String,
    /// 会话 ID。
    pub conversation_id: String,
    /// 增量片段（流式响应用）。
    pub deltas: Vec<String>,
    /// 原始 SSE（诊断用，调用方可丢弃）。
    pub sse_raw: String,
}

/// 生成一次文本回复。
pub async fn generate_text(
    client: &Client,
    token: &str,
    request: &TextRequest,
    deadline: Deadline,
) -> Result<(TextResult, FlowDiagnostics)> {
    let Handshake {
        requirements_token,
        proof_token,
        mut diag,
    } = handshake(client, token, &request.user_agent, deadline).await?;

    let budget = stage_budget(deadline, "sse", stage_caps::SSE)?;
    let sse = client
        .post_sse(
            &upstream_url(CONVERSATION),
            // 文本链路**不带** conduit_token（那是图片链路才有的）
            &image_headers(
                &request.user_agent,
                token,
                CONVERSATION,
                &requirements_token,
                &proof_token,
                None,
                "text/event-stream",
            ),
            &if request.messages.is_empty() {
                build_text_conversation_payload(&request.prompt, &request.upstream_model)
            } else {
                build_text_conversation_payload_with_images(
                    &request.messages,
                    &request.images,
                    &request.upstream_model,
                )
            },
            Some(budget),
        )
        .await?;
    diag.sse_status = sse.status;
    diag.sse_bytes = sse.body.len();
    if sse.status != 200 {
        // 必须带分类：上层靠 kind 决定换号 / 标异常，裸字符串会让这些判断失效
        return Err(GenerateError::new(
            classify_http(sse.status, &sse.body),
            format!(
                "文本 SSE 请求失败，状态 {}: {}",
                sse.status,
                clip(&sse.body)
            ),
        )
        .into());
    }

    let (text, conversation_id, deltas) = collect_text(&sse.body, request.history());
    if text.trim().is_empty() {
        // 空回复要能诊断：把 SSE 的头尾留下来
        return Err(GenerateError::new(
            FailureKind::EmptyReply,
            format!(
                "上游没有返回文本（SSE {} 字节）：{}",
                sse.body.len(),
                clip(&sse.body)
            ),
        )
        .into());
    }

    Ok((
        TextResult {
            text,
            conversation_id,
            deltas,
            sse_raw: sse.body,
        },
        diag,
    ))
}

/// 流式生成：边收边吐增量。
///
/// 目前上游是一次性把 SSE 给全的（`post_sse` 读完才返回），
/// 所以这里先把增量切成小块吐出去 —— 对客户端来说时序仍然比「全等」好。
pub fn stream_deltas(result: &TextResult, chunk_len: usize) -> Vec<String> {
    if chunk_len == 0 || result.text.is_empty() {
        return vec![result.text.clone()];
    }
    result
        .text
        .chars()
        .collect::<Vec<_>>()
        .chunks(chunk_len)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpt2apirs_protocol::text_stream::TextAccumulator;

    #[test]
    fn default_request_is_text_model() {
        let request = TextRequest::new("hi");
        assert_eq!(request.model, "gpt-5-5");
        assert_eq!(request.upstream_model, "gpt-5-5");
        assert!(request.history().is_empty());
    }

    #[test]
    fn multi_turn_request_derives_replay_history() {
        // 调用方只给消息，回放判定所需的历史正文必须自动带上
        let request = TextRequest::new(String::new()).with_messages(vec![
            ("user".to_string(), "问".to_string()),
            ("assistant".to_string(), "上一答".to_string()),
            ("user".to_string(), "再问".to_string()),
        ]);
        assert_eq!(request.history(), ["上一答".to_string()]);
    }

    #[test]
    fn stream_deltas_splits_text() {
        let result = TextResult {
            text: "abcdef".to_string(),
            ..Default::default()
        };
        assert_eq!(stream_deltas(&result, 2), vec!["ab", "cd", "ef"]);
        assert_eq!(stream_deltas(&result, 0), vec!["abcdef"]);
    }

    #[test]
    fn stream_deltas_handles_empty_text() {
        let result = TextResult::default();
        assert_eq!(stream_deltas(&result, 4), vec![String::new()]);
    }

    #[test]
    fn accumulator_round_trip_from_synthetic_sse() {
        // 纯逻辑：不联网，只验证「SSE 文本  最终文本」这条路
        let sse = concat!(
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"你好\"}]}\n\n",
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"，世界\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (text, _conversation_id, deltas) = collect_text(sse, &[]);
        assert_eq!(text, "你好，世界");
        assert_eq!(deltas.len(), 2);
    }

    #[test]
    fn accumulator_is_reusable_type() {
        // 保证公开类型能被外部构造（文档示例依赖）
        let mut accumulator = TextAccumulator::new();
        assert!(accumulator.push_payload("[DONE]").is_none());
        assert!(accumulator.done);
    }
}

// 真流式：边收边吐

/// 流式生成结束后可读的最终状态。
#[derive(Debug, Default, Clone)]
pub struct StreamSnapshot {
    /// 完整文本（已清洗）。
    pub text: String,
    /// 会话 ID。
    pub conversation_id: String,
    /// 是否已正常结束。
    pub finished: bool,
}

/// 一次流式生成。
pub struct TextStream {
    /// 增量文本流。每项是一个新增片段。
    pub deltas: futures_util::stream::BoxStream<
        'static,
        std::result::Result<String, GenerateError>,
    >,
    /// 结束后读全文（用于算 usage / 记日志）。
    pub snapshot: std::sync::Arc<std::sync::Mutex<StreamSnapshot>>,
    /// 握手诊断。
    pub diag: FlowDiagnostics,
}

/// 流式生成文本。
///
/// 与 [`generate_text`] 的区别：**握手之后立刻把字节流交出去**，
/// 每收到一个完整 SSE 事件就把新增文本吐一个片段，
/// 而不是等上游把整段写完再返回。
pub async fn generate_text_stream(
    client: &Client,
    token: &str,
    request: &TextRequest,
    deadline: Deadline,
) -> std::result::Result<TextStream, GenerateError> {
    use futures_util::StreamExt;

    let Handshake {
        requirements_token,
        proof_token,
        diag,
    } = handshake(client, token, &request.user_agent, deadline).await?;

    let budget = stage_budget(deadline, "sse", stage_caps::SSE)
        .map_err(|error| GenerateError::new(FailureKind::Network, error.to_string()))?;
    // 先落到局部变量，避免临时数组在调用期间被释放
    let messages: Vec<(String, String)> = if request.messages.is_empty() {
        vec![("user".to_string(), request.prompt.clone())]
    } else {
        request.messages.clone()
    };
    let payload = build_text_conversation_payload_with_images(
        &messages,
        &request.images,
        &request.upstream_model,
    );

    let started = client
        .post_sse_stream(
            &upstream_url(CONVERSATION),
            &image_headers(
                &request.user_agent,
                token,
                CONVERSATION,
                &requirements_token,
                &proof_token,
                None,
                "text/event-stream",
            ),
            &payload,
            Some(budget),
        )
        .await
        .map_err(|error| GenerateError::new(FailureKind::Network, error.to_string()))?;

    if started.status != 200 {
        return Err(GenerateError::new(
            classify_http(started.status, &started.body_prefix),
            format!(
                "文本 SSE 请求失败，状态 {}: {}",
                started.status,
                clip(&started.body_prefix)
            ),
        ));
    }
    let Some(bytes) = started.stream else {
        return Err(GenerateError::new(
            FailureKind::Protocol,
            "上游返回 200 但没有响应体",
        ));
    };

    let snapshot = std::sync::Arc::new(std::sync::Mutex::new(StreamSnapshot::default()));
    let history = request.history().to_vec();

    // 状态：字节流 + 分帧器 + 累积器 + 待发片段
    struct State {
        bytes: futures_util::stream::BoxStream<'static, anyhow::Result<Vec<u8>>>,
        framer: gpt2apirs_protocol::sse_framer::SseFramer,
        accumulator: TextAccumulator,
        pending: std::collections::VecDeque<String>,
        snapshot: std::sync::Arc<std::sync::Mutex<StreamSnapshot>>,
        finished: bool,
    }

    let state = State {
        bytes,
        framer: gpt2apirs_protocol::sse_framer::SseFramer::new(),
        accumulator: TextAccumulator::with_history(history),
        pending: std::collections::VecDeque::new(),
        snapshot: snapshot.clone(),
        finished: false,
    };

    let deltas = futures_util::stream::unfold(state, |mut state| async move {
        loop {
            // 先吐积压的片段
            if let Some(delta) = state.pending.pop_front() {
                return Some((Ok(delta), state));
            }
            if state.finished {
                // 收尾时把最终文本写回快照 —— 上游发过 [DONE] 时走的就是这条路，
                // 漏了这一步会让上层算出的 completion_tokens 永远是 0
                {
                    let mut guard = state.snapshot.lock().unwrap_or_else(|p| p.into_inner());
                    if guard.text.is_empty() {
                        guard.text = state.accumulator.text.clone();
                        guard.conversation_id = state.accumulator.conversation_id.clone();
                        guard.finished = true;
                    }
                }
                return None;
            }

            match state.bytes.next().await {
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    let payloads = state.framer.push(&text);
                    for payload in payloads {
                        if let Some(delta) = state.accumulator.push_payload(&payload) {
                            state.pending.push_back(delta);
                        }
                        if state.accumulator.done {
                            state.finished = true;
                        }
                    }
                }
                Some(Err(error)) => {
                    state.finished = true;
                    {
                        let mut guard = state.snapshot.lock().unwrap_or_else(|p| p.into_inner());
                        guard.text = state.accumulator.text.clone();
                        guard.conversation_id = state.accumulator.conversation_id.clone();
                    }
                    return Some((
                        Err(GenerateError::new(
                            FailureKind::Network,
                            format!("读取流失败：{error}"),
                        )),
                        state,
                    ));
                }
                None => {
                    // 流结束：把缓冲区残余当最后一条事件
                    if let Some(payload) = state.framer.finish() {
                        if let Some(delta) = state.accumulator.push_payload(&payload) {
                            state.pending.push_back(delta);
                        }
                    }
                    {
                        let mut guard = state.snapshot.lock().unwrap_or_else(|p| p.into_inner());
                        guard.text = state.accumulator.text.clone();
                        guard.conversation_id = state.accumulator.conversation_id.clone();
                        guard.finished = true;
                    }
                    state.finished = true;
                    if state.pending.is_empty() {
                        return None;
                    }
                }
            }
        }
    })
    .boxed();

    Ok(TextStream {
        deltas,
        snapshot,
        diag,
    })
}

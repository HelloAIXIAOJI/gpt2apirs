//! 联网搜索的执行链路。
//!
//! 流程：handshake → prepare（带 search 提示）→ conversation（SSE 发起搜索）
//! → 轮询会话详情直到出答案。答案与来源都在会话详情里，不在 SSE 里。

use std::time::Duration;

use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::conversation::parse_conversation_detail;
use gpt2apirs_protocol::endpoints::{CONVERSATION, CONVERSATION_PREPARE, url as upstream_url};
use gpt2apirs_protocol::search::{
    build_search_conversation_payload, build_search_prepare_payload, parse_search_result,
};
use gpt2apirs_protocol::text_stream::collect_text;

use crate::error::{GenerateError, FailureKind, classify_http};
use crate::DEFAULT_USER_AGENT;
use crate::{handshake, image_headers, stage_budget};

/// 轮询会话详情的间隔与上限。
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const POLL_TIMEOUT_SECS: u64 = 300;

/// 一次搜索的结果。
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    /// 会话 ID。
    pub conversation_id: String,
    /// 答案正文（注解已清洗）。
    pub answer: String,
    /// 来源列表。
    pub sources: Vec<gpt2apirs_protocol::search::SearchSource>,
    /// 会话结束状态。
    pub status: String,
}

/// 执行一次联网搜索。
pub async fn search(
    client: &Client,
    token: &str,
    query: &str,
    model: &str,
    deadline: Deadline,
) -> Result<SearchOutcome, GenerateError> {
    let handshaken = handshake(client, token, DEFAULT_USER_AGENT, deadline).await?;

    // 1) prepare：拿 conduit_token（搜索会话必须有）
    let budget = stage_budget(deadline, "search_prepare", Duration::from_secs(60))
        .map_err(|error| GenerateError::new(FailureKind::Network, error.to_string()))?;
    let prepare = client
        .post_json(
            &upstream_url(CONVERSATION_PREPARE),
            &gpt2apirs_http::api_headers(DEFAULT_USER_AGENT, token, CONVERSATION_PREPARE),
            &build_search_prepare_payload(query, model),
            Some(budget),
        )
        .await
        .map_err(|error| {
            GenerateError::new(FailureKind::Network, format!("搜索 prepare 失败: {error}"))
        })?;
    if prepare.status != 200 {
        return Err(GenerateError::new(
            classify_http(prepare.status, &prepare.body),
            format!("搜索 prepare 返回 {}", prepare.status),
        ));
    }
    let prepare_body: serde_json::Value = serde_json::from_str(&prepare.body).unwrap_or_default();
    let conduit = gpt2apirs_protocol::conversation::parse_conduit_token(&prepare_body)
        .ok_or_else(|| GenerateError::new(FailureKind::Protocol, "搜索 prepare 未返回 conduit_token"))?;

    // 2) conversation：发起搜索，SSE 里只用来取会话 ID
    let budget = stage_budget(deadline, "search_sse", Duration::from_secs(120))
        .map_err(|error| GenerateError::new(FailureKind::Network, error.to_string()))?;
    let sse = client
        .post_sse(
            &upstream_url(CONVERSATION),
            &image_headers(
                DEFAULT_USER_AGENT,
                token,
                CONVERSATION,
                &handshaken.requirements_token,
                &handshaken.proof_token,
                Some(&conduit),
                "text/event-stream",
            ),
            &build_search_conversation_payload(query, model),
            Some(budget),
        )
        .await
        .map_err(|error| {
            GenerateError::new(FailureKind::Network, format!("搜索会话请求失败: {error}"))
        })?;
    if sse.status != 200 {
        return Err(GenerateError::new(
            classify_http(sse.status, &sse.body),
            format!("搜索会话返回 {}", sse.status),
        ));
    }
    let (_text, conversation_id, _deltas) = collect_text(&sse.body, "");
    if conversation_id.is_empty() {
        return Err(GenerateError::new(
            FailureKind::Protocol,
            "搜索 SSE 里没有 conversation_id",
        ));
    }

    // 3) 轮询会话详情：答案与来源都在那里
    let poll_deadline = deadline
        .remaining()
        .unwrap_or(Duration::from_secs(POLL_TIMEOUT_SECS))
        .min(Duration::from_secs(POLL_TIMEOUT_SECS))
        .max(Duration::from_secs(1));
    let started = std::time::Instant::now();
    let mut last_status = String::new();
    while started.elapsed() < poll_deadline {
        match crate::conversation::fetch_conversation_payload(
            client,
            token,
            DEFAULT_USER_AGENT,
            &conversation_id,
            deadline,
        )
        .await
        {
            Ok(payload) => {
                let result = parse_search_result(&payload);
                last_status = result.status.clone();
                // 有答案且会话已结束 → 收工；只有答案没结束也先返回（轮询上限内不再等）
                if !result.answer.trim().is_empty() {
                    return Ok(SearchOutcome {
                        conversation_id,
                        answer: result.answer,
                        sources: result.sources,
                        status: result.status,
                    });
                }
            }
            // 会话刚建好时可能短暂 404，继续等
            Err(error) if error.kind == FailureKind::Network || error.kind == FailureKind::Protocol => {
                last_status = format!("{}", error.message);
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let _ = parse_conversation_detail(&serde_json::Value::Null);
    Err(GenerateError::new(
        FailureKind::Protocol,
        format!("搜索超时未拿到答案（最后状态: {last_status}）"),
    ))
}

//! 会话详情抓取（`GET /backend-api/conversation/{id}`）。
//!
//! 与生成链路分开：这是个只读 GET，不需要 sentinel / prepare，
//! 也不需要 conduit token；带上 Referer 和 Target-Route 即可。

use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::conversation::{parse_conversation_detail, ConversationDetail};
use gpt2apirs_protocol::endpoints::BASE_URL;

use crate::error::{GenerateError, classify_http};
use crate::stage_budget;

/// 抓会话详情。
///
/// 非 200 一律按 HTTP 分类成 [`GenerateError`]，由调用方决定是否换号重试。
pub async fn fetch_conversation_payload(
    client: &Client,
    token: &str,
    user_agent: &str,
    conversation_id: &str,
    deadline: Deadline,
) -> Result<serde_json::Value, GenerateError> {
    let path = gpt2apirs_protocol::endpoints::conversation(conversation_id);
    let budget = stage_budget(deadline, "conversation_detail", std::time::Duration::from_secs(60))
        .map_err(|error| GenerateError::new(crate::error::FailureKind::Network, error.to_string()))?;
    // 用统一的 API 头（浏览器指纹 + Bearer + OAI 头），只把 Referer 换成会话页：
    // 上游按 Referer 判断请求来源，缺指纹头会直接 401。
    let mut headers: Vec<(&'static str, String)> =
        gpt2apirs_http::api_headers(user_agent, token, &path);
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("referer"));
    headers.push(("Referer", format!("{BASE_URL}/c/{conversation_id}")));
    let response = client
        .get_text(&format!("{BASE_URL}{path}"), &headers, Some(budget))
        .await
        .map_err(|error| {
            GenerateError::new(crate::error::FailureKind::Network, format!("会话详情请求失败: {error}"))
        })?;
    if response.status != 200 {
        return Err(GenerateError::new(
            classify_http(response.status, &response.body),
            format!("会话详情返回 {}", response.status),
        ));
    }
    let payload: serde_json::Value = serde_json::from_str(&response.body).map_err(|error| {
        GenerateError::new(
            crate::error::FailureKind::Protocol,
            format!("会话详情不是合法 JSON: {error}"),
        )
    })?;
    Ok(payload)
}

/// 抓会话详情并解析成结构。
pub async fn fetch_conversation_detail(
    client: &Client,
    token: &str,
    user_agent: &str,
    conversation_id: &str,
    deadline: Deadline,
) -> Result<ConversationDetail, GenerateError> {
    let payload = fetch_conversation_payload(client, token, user_agent, conversation_id, deadline).await?;
    Ok(parse_conversation_detail(&payload))
}

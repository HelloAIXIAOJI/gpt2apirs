//! 语音：转写与合成。
//!
//! 上游`synthesize` 只能合成**已存在的消息**，所以 TTS 是两跳：
//! 先让模型把文字原样复述成一条 assistant 消息，再对那条消息合成音频。
//! 代价是真的多消耗一轮生成额度，而且会在账号里留下这轮对话 ——
//! 合成完要把它删掉，否则会话页会被 TTS 请求堆满。

use anyhow::{Context, Result, bail};

use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::endpoints;
use gpt2apirs_protocol::speech::{latest_assistant_message_id, transcript};

use crate::stage_budget;
use crate::text::{TextRequest, generate_text};
use gpt2apirs_http::{DEFAULT_USER_AGENT, api_headers};

/// 语音链路给单阶段留的时间上限。
const SPEECH_CAP: std::time::Duration = std::time::Duration::from_secs(90);

/// 刚生成的对话上游可能还没落库：详情要重试几次。
const DETAIL_ATTEMPTS: u32 = 5;

const DETAIL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);

fn headers(token: &str, path: &str) -> Vec<(&'static str, String)> {
    api_headers(DEFAULT_USER_AGENT, token, path)
}

/// 转写一段音频。
pub async fn transcribe(
    client: &Client,
    token: &str,
    filename: &str,
    content_type: &str,
    data: &[u8],
    deadline: Deadline,
) -> Result<String> {
    let boundary = "----gpt2apirsSpeechBoundary";
    let safe_name = filename.replace(['"', '\r', '\n'], "_");
    let mut body: Vec<u8> = Vec::with_capacity(data.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let url = endpoints::url(endpoints::TRANSCRIBE);
    let response = client
        .post_raw(
            &url,
            &headers(token, endpoints::TRANSCRIBE),
            &format!("multipart/form-data; boundary={boundary}"),
            body,
            Some(stage_budget(deadline, "语音转写", SPEECH_CAP)?),
        )
        .await
        .context("POST /backend-api/transcribe 失败")?;
    if !(200..300).contains(&response.status) {
        bail!(
            "转写返回 {}: {}",
            response.status,
            response.body.chars().take(200).collect::<String>()
        );
    }
    let body: serde_json::Value =
        serde_json::from_str(&response.body).with_context(|| "转写响应不是 JSON")?;
    transcript(&body).ok_or_else(|| anyhow::anyhow!("转写响应里没有 text 字段"))
}

/// 对一条已存在的消息合成音频。
pub async fn synthesize(
    client: &Client,
    token: &str,
    conversation_id: &str,
    message_id: &str,
    voice: &str,
    format: &str,
    deadline: Deadline,
) -> Result<(Vec<u8>, String)> {
    let path = endpoints::synthesize(conversation_id, message_id, voice, format);
    let url = endpoints::url(&path);
    let response = client
        .get_bytes(
            &url,
            &headers(token, "/backend-api/synthesize"),
            Some(stage_budget(deadline, "语音合成", SPEECH_CAP)?),
        )
        .await
        .context("GET /backend-api/synthesize 失败")?;
    if !(200..300).contains(&response.status) {
        let text = String::from_utf8_lossy(&response.body);
        bail!("合成返回 {}: {}", response.status, text.chars().take(200).collect::<String>());
    }
    // 上游有时不给 content-type；按请求的格式推导更可靠
    Ok((
        response.body,
        gpt2apirs_protocol::speech::audio_content_type(format).to_string(),
    ))
}

/// 把任意文字变成音频（两跳）。
///
/// **一定会回收**那轮复述对话：成功失败都删。
/// 失败路径也要删 —— 否则每次失败都会在账号里留一条垃圾会话（实测漏过三次）。
/// 所以 `conversation_id` 通过出参带出来，由这里的 `finally` 统一处理。
pub async fn speak(
    client: &Client,
    token: &str,
    text: &str,
    voice: &str,
    format: &str,
    deadline: Deadline,
) -> Result<(Vec<u8>, String)> {
    // 用出参把 id 带出来：`?` 提前返回时调用方拿不到返回值，但拿得到这个
    let mut touched: Option<String> = None;
    let outcome = speak_inner(client, token, text, voice, format, deadline, &mut touched).await;
    if let Some(conversation_id) = touched {
        if let Err(error) = gpt2apirs_generate_conversation_delete(client, token, &conversation_id)
            .await
        {
            tracing::warn!(
                conversation_id = %conversation_id,
                error = %error,
                "TTS 中间对话删除失败（会留在账号里）"
            );
        }
    }
    outcome
}

async fn gpt2apirs_generate_conversation_delete(
    client: &Client,
    token: &str,
    conversation_id: &str,
) -> Result<()> {
    let path = endpoints::conversation_delete(conversation_id);
    let url = endpoints::url(&path);
    let response = client
        .delete_text(&url, &headers(token, &path), None)
        .await
        .context("DELETE 会话失败")?;
    if !(200..300).contains(&response.status) {
        bail!("删除会话返回 {}", response.status);
    }
    Ok(())
}

async fn speak_inner(
    client: &Client,
    token: &str,
    text: &str,
    voice: &str,
    format: &str,
    deadline: Deadline,
    touched: &mut Option<String>,
) -> Result<(Vec<u8>, String)> {
    // 第 1 跳：让它原样复述，产出一条可用于合成的 assistant 消息
    let prompt = format!(
        "请原样输出下面这段文字，一个字都不要改写、增删或解释，直接输出正文：\n\n{text}"
    );
    // 必须落库：临时对话查详情是 404，合成就拿不到消息
    let request = TextRequest::new(prompt).with_persist();
    let (result, _diag) = generate_text(client, token, &request, deadline)
        .await
        .map_err(|error| anyhow::anyhow!("复述失败：{error}"))?;
    let conversation_id = result.conversation_id.clone();
    // 从这里开始，这轮对话已经建出来了：无论后面成败都要回收
    *touched = Some(conversation_id.clone());

    // 第 2 跳：要消息 id，只能问会话详情。
    //
    // 刚生成的对话上游可能还没落库，第一次查会 404 —— 实测必现。
    // 重试几次就够了，不值得为它去改 SSE 解析把 message_id 一路带出来。
    let detail_path = endpoints::conversation_detail(&conversation_id);
    let detail_url = endpoints::url(&detail_path);
    let mut detail_body = None;
    for attempt in 1..=DETAIL_ATTEMPTS {
        let response = client
            .get_text(
                &detail_url,
                &headers(token, &detail_path),
                Some(stage_budget(deadline, "会话详情", SPEECH_CAP)?),
            )
            .await
            .context("GET 会话详情失败")?;
        if (200..300).contains(&response.status) {
            detail_body = Some(response.body);
            break;
        }
        if attempt == DETAIL_ATTEMPTS {
            bail!("会话详情连续 {} 次返回 {}", DETAIL_ATTEMPTS, response.status);
        }
        tokio::time::sleep(DETAIL_RETRY_DELAY).await;
    }
    let detail_body = detail_body.ok_or_else(|| anyhow::anyhow!("拿不到会话详情"))?;
    let detail: serde_json::Value =
        serde_json::from_str(&detail_body).context("会话详情不是 JSON")?;
    if latest_assistant_message_id(&detail).is_none() {
        // 结构对不上时把节点摘要打出来：只看 role/id 就能判断是没生成完还是字段不同
        let nodes: Vec<String> = detail
            .get("mapping")
            .and_then(serde_json::Value::as_object)
            .map(|map| {
                map.values()
                    .filter_map(|node| {
                        let message = node.get("message")?;
                        let role = message
                            .pointer("/author/role")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?");
                        let id = message
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?");
                        let created = message
                            .get("create_time")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0);
                        Some(format!("{role}:{id}@{created:.1}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let keys: Vec<&str> = detail
            .as_object()
            .map(|map| map.keys().map(String::as_str).collect())
            .unwrap_or_default();
        tracing::warn!(
            nodes = %nodes.join(" | "),
            top_level = %keys.join(","),
            "TTS 详情里找不到 assistant 消息"
        );
    }
    let message_id = latest_assistant_message_id(&detail)
        .ok_or_else(|| anyhow::anyhow!("会话里没有可用于合成的 assistant 消息"))?;

    let (bytes, content_type) = synthesize(
        client,
        token,
        &conversation_id,
        &message_id,
        voice,
        format,
        deadline,
    )
    .await?;
    Ok((bytes, content_type))
}

//! 账号维度的上游数据：官网会话与「记忆」的列表 / 删除。
//!
//! 这些操作不建会话、不花配额，跟 images / text 两条流水线无关，所以单独放一个模块。
//! 端点与字段形状都是实测的，改上游就照这里一起改。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use gpt2apirs_core::Deadline;
use gpt2apirs_http::{Client, DEFAULT_USER_AGENT, api_headers};
use gpt2apirs_protocol::conversations::{ConversationPage, parse_page};
use gpt2apirs_protocol::endpoints;
use gpt2apirs_protocol::memories::{
    MemoryPage, parse as parse_memories, parse_memory_enabled,
};
use serde_json::Value;

/// 单次请求的上限：列表/删除都不该慢，慢了就是上游在卡我们。
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

fn budget(deadline: Deadline) -> Duration {
    match deadline.remaining() {
        Some(left) => left.min(CALL_TIMEOUT),
        None => CALL_TIMEOUT,
    }
}

/// 上游要求的 target path 只认路径本身，带查询串会被判成非法来源。
fn target_path(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn headers(token: &str, path: &str) -> Vec<(&'static str, String)> {
    api_headers(DEFAULT_USER_AGENT, token, target_path(path))
}

async fn get_json(client: &Client, token: &str, path: &str, deadline: Deadline) -> Result<Value> {
    let url = endpoints::url(path);
    let response = client
        .get_text(&url, &headers(token, path), Some(budget(deadline)))
        .await
        .with_context(|| format!("GET {} 失败", target_path(path)))?;
    if response.status != 200 {
        bail!(
            "{} 返回 {}: {}",
            target_path(path),
            response.status,
            clip(&response.body)
        );
    }
    serde_json::from_str(&response.body)
        .with_context(|| format!("{} 响应不是 JSON", target_path(path)))
}

async fn patch(client: &Client, token: &str, path: &str, body: Option<&str>, deadline: Deadline) -> Result<u16> {
    let url = endpoints::url(path);
    let response = client
        .patch_text(&url, &headers(token, path), body, Some(budget(deadline)))
        .await
        .with_context(|| format!("PATCH {} 失败", target_path(path)))?;
    Ok(response.status)
}

async fn delete(client: &Client, token: &str, path: &str, deadline: Deadline) -> Result<u16> {
    let url = endpoints::url(path);
    let response = client
        .delete_text(&url, &headers(token, path), Some(budget(deadline)))
        .await
        .with_context(|| format!("DELETE {} 失败", target_path(path)))?;
    Ok(response.status)
}

/// 列官网会话（按更新时间倒序）。
///
/// 实测：GET /backend-api/conversations?offset&limit&order=updated 返回 {items,total,limit,offset}。
pub async fn list_conversations(
    client: &Client,
    token: &str,
    offset: i64,
    limit: i64,
    deadline: Deadline,
) -> Result<ConversationPage> {
    let path = endpoints::conversations_page(offset, limit);
    let body = get_json(client, token, &path, deadline).await?;
    Ok(parse_page(&body))
}

/// 删一条会话。
///
/// 返回 Ok(false) 表示上游说「没这条」（404）—— 页面按「已经没了」处理，
/// 不当失败：重复点删除是常事。
pub async fn delete_conversation(
    client: &Client,
    token: &str,
    conversation_id: &str,
    deadline: Deadline,
) -> Result<bool> {
    let path = endpoints::conversation_delete(conversation_id);
    match delete(client, token, &path, deadline).await? {
        200..=299 => Ok(true),
        404 => Ok(false),
        status => bail!("删除会话返回 {status}"),
    }
}

/// 列已保存的记忆。
///
/// 实测：GET /backend-api/memories 返回 {memories, memory_max_tokens, memory_num_tokens}。
pub async fn list_memories(
    client: &Client,
    token: &str,
    deadline: Deadline,
) -> Result<MemoryPage> {
    let body = get_json(client, token, endpoints::MEMORIES, deadline).await?;
    Ok(parse_memories(&body))
}

/// 删一条记忆。
///
/// 上游对任意 id 都回 success（不校验存在性），所以这里分不出「真的删了」
/// 和「本来就没有」—— 页面刷新后以列表为准。
pub async fn delete_memory(
    client: &Client,
    token: &str,
    memory_id: &str,
    deadline: Deadline,
) -> Result<()> {
    let path = endpoints::memory_delete(memory_id);
    match delete(client, token, &path, deadline).await? {
        200..=299 => Ok(()),
        404 => Ok(()),
        status => bail!("删除记忆返回 {status}"),
    }
}

/// 读账号的记忆总开关。`None` = 读不到（未知），不是「关着」。
pub async fn memory_enabled(client: &Client, token: &str, deadline: Deadline) -> Result<Option<bool>> {
    let body = get_json(client, token, endpoints::USER_SETTINGS, deadline).await?;
    Ok(parse_memory_enabled(&body))
}

/// 改记忆总开关。已保存的记忆不受影响。
pub async fn set_memory_enabled(
    client: &Client,
    token: &str,
    enabled: bool,
    deadline: Deadline,
) -> Result<()> {
    let path = endpoints::memory_setting(enabled);
    match patch(client, token, &path, Some("{}"), deadline).await? {
        200..=299 => Ok(()),
        status => bail!("设置记忆开关返回 {status}"),
    }
}

/// 清空这个账号已保存的记忆（不动开关）。
pub async fn clear_saved_memories(client: &Client, token: &str, deadline: Deadline) -> Result<()> {
    match delete(client, token, endpoints::MEMORY_CLEAR, deadline).await? {
        200..=299 => Ok(()),
        status => bail!("清空记忆返回 {status}"),
    }
}

fn clip(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() > 160 {
        format!("{}…", one_line.chars().take(160).collect::<String>())
    } else {
        one_line
    }
}

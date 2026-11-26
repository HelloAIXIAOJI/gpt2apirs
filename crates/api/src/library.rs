//! 官网会话 / 记忆的读取与删除：选号 + 调用上游 + 错误映射。
//!
//! JSON 接口（/api/*）与后台页面共用这里，免得两边各写一遍选号逻辑。

use std::sync::Arc;

use gpt2apirs_account::{PoolError, Purpose, SelectionFilter};
use gpt2apirs_core::{AppError, Deadline};
use gpt2apirs_protocol::conversations::ConversationPage;
use gpt2apirs_protocol::memories::MemoryPage;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// 一次读取的总时限：这些接口都是「看一眼官网」，不该占着账号太久。
const READ_TIMEOUT_SECS: f64 = 25.0;

/// 语音要走两跳（复述 + 合成），给的时间长一些。
const SPEECH_TIMEOUT_SECS: f64 = 180.0;

fn unavailable(message: impl Into<String>, code: &str) -> ApiError {
    ApiError::admin(AppError::ServiceUnavailable {
        message: message.into(),
        code: code.to_string(),
    })
}

/// 后台下拉框用的账号清单（只给还在正常状态的号）。
pub async fn selectable_accounts(state: &Arc<AppState>) -> Vec<(Uuid, String)> {
    let repo = state.pool.repo();
    match repo.list_all().await {
        Ok(accounts) => accounts
            .into_iter()
            .filter(|account| account.status == gpt2apirs_account::AccountStatus::Normal)
            .map(|account| {
                let label = if account.email.as_deref().unwrap_or("").trim().is_empty() {
                    account.id.to_string().chars().take(8).collect()
                } else {
                    account.email.clone().unwrap_or_default()
                };
                (account.id, label)
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "读取账号清单失败");
            Vec::new()
        }
    }
}

fn filter_for(account: Option<Uuid>) -> SelectionFilter {
    SelectionFilter {
        purpose: Purpose::Text,
        only: account,
        ..Default::default()
    }
}

fn lease_error(error: PoolError) -> ApiError {
    match error {
        PoolError::NoCandidate { reason } => unavailable(
            format!("没有可用于查询的账号（{reason}）"),
            "no_available_account",
        ),
        PoolError::DeadlineExceeded => unavailable("等账号槽位超时，请稍后重试", "account_pool_timeout"),
        other => ApiError::admin(AppError::Internal(anyhow::anyhow!("{other}"))),
    }
}

fn upstream_error(error: anyhow::Error) -> ApiError {
    // 账号凭证失效要能一眼看出来，别混成「未知错误」
    let text = error.to_string();
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("返回 401") || lowered.contains("返回 403") {
        return ApiError::admin(AppError::ServiceUnavailable {
            message: format!("上游拒绝了这个账号的凭据：{text}"),
            code: "account_token_invalid".to_string(),
        });
    }
    ApiError::admin(AppError::Internal(anyhow::anyhow!(text)))
}

/// 读一页会话，返回（数据, 实际用的账号）。
pub async fn conversations(
    state: &Arc<AppState>,
    account: Option<Uuid>,
    offset: i64,
    limit: i64,
) -> Result<(ConversationPage, Uuid), ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        // 出口统一走全局（账号已无代理字段）
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let page = gpt2apirs_generate::account_data::list_conversations(
        &client,
        lease.access_token(),
        offset,
        limit,
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok((page, lease.id()))
}

/// 删一条会话，返回（上游是否认得这条, 实际用的账号）。
pub async fn delete_conversation(
    state: &Arc<AppState>,
    account: Option<Uuid>,
    conversation_id: &str,
) -> Result<(bool, Uuid), ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        // 出口统一走全局（账号已无代理字段）
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let removed = gpt2apirs_generate::account_data::delete_conversation(
        &client,
        lease.access_token(),
        conversation_id,
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok((removed, lease.id()))
}

/// 读记忆，返回（数据, 实际用的账号）。
pub async fn memories(
    state: &Arc<AppState>,
    account: Option<Uuid>,
) -> Result<(MemoryPage, Uuid), ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        // 出口统一走全局（账号已无代理字段）
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let page = gpt2apirs_generate::account_data::list_memories(&client, lease.access_token(), deadline)
        .await
        .map_err(upstream_error)?;
    Ok((page, lease.id()))
}

/// 读这个账号的记忆总开关。`None` = 上游没给，未知（不是「关着」）。
pub async fn memory_enabled(
    state: &Arc<AppState>,
    account: Option<Uuid>,
) -> Result<(Option<bool>, Uuid), ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let enabled = gpt2apirs_generate::account_data::memory_enabled(
        &client,
        lease.access_token(),
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok((enabled, lease.id()))
}

/// 改记忆总开关（不清已有记忆）。
pub async fn set_memory_enabled(
    state: &Arc<AppState>,
    account: Option<Uuid>,
    enabled: bool,
) -> Result<Uuid, ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    gpt2apirs_generate::account_data::set_memory_enabled(
        &client,
        lease.access_token(),
        enabled,
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok(lease.id())
}

/// 清空这个账号已保存的记忆。
pub async fn clear_memories(
    state: &Arc<AppState>,
    account: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    gpt2apirs_generate::account_data::clear_saved_memories(
        &client,
        lease.access_token(),
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok(lease.id())
}

/// 删一条记忆。
pub async fn delete_memory(
    state: &Arc<AppState>,
    account: Option<Uuid>,
    memory_id: &str,
) -> Result<Uuid, ApiError> {
    let deadline = Deadline::in_secs(READ_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(account), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        // 出口统一走全局（账号已无代理字段）
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    gpt2apirs_generate::account_data::delete_memory(
        &client,
        lease.access_token(),
        memory_id,
        deadline,
    )
    .await
    .map_err(upstream_error)?;
    Ok(lease.id())
}

/// 文本转语音（两跳：先复述成消息，再合成）。
///
/// 顺带把中间那轮对话删掉 —— 否则每次 TTS 都会在账号里留一条
/// 「请原样输出下面这段文字」的垃圾会话，把会话页刷满。
pub async fn speak(
    state: &Arc<AppState>,
    text: &str,
    voice: &str,
    format: &str,
) -> Result<(Vec<u8>, String), ApiError> {
    let deadline = Deadline::in_secs(SPEECH_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(None), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    // 中间那轮复述对话由 speech::speak 自己回收（成功失败都删）
    gpt2apirs_generate::speech::speak(
        &client,
        lease.access_token(),
        text,
        voice,
        format,
        deadline,
    )
    .await
    .map_err(upstream_error)
}

/// 语音转文字。
pub async fn transcribe(
    state: &Arc<AppState>,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> Result<String, ApiError> {
    let deadline = Deadline::in_secs(SPEECH_TIMEOUT_SECS);
    let lease = state
        .pool
        .acquire(&filter_for(None), deadline)
        .await
        .map_err(lease_error)?;
    let client = state
        .client_for("")
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    gpt2apirs_generate::speech::transcribe(
        &client,
        lease.access_token(),
        filename,
        content_type,
        data,
        deadline,
    )
    .await
    .map_err(upstream_error)
}

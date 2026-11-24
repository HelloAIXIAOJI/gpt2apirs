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
        .client_for(&lease.account().proxy)
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
        .client_for(&lease.account().proxy)
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
        .client_for(&lease.account().proxy)
        .map_err(|error| ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let page = gpt2apirs_generate::account_data::list_memories(&client, lease.access_token(), deadline)
        .await
        .map_err(upstream_error)?;
    Ok((page, lease.id()))
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
        .client_for(&lease.account().proxy)
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

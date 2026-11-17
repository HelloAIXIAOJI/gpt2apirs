//! `POST /v1/search` —— 联网搜索。
//!
//! 搜索走独立链路（带 search 提示的会话 + 轮询），不经过文本缓存：
//! 结果是时效性内容，缓存住反而会给出过期答案。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use gpt2apirs_account::{PoolError, SelectionFilter};
use gpt2apirs_core::{AppError, Deadline};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::routes::text_common::resolve_model;
use crate::state::AppState;

/// 搜索的时限。比生图长：上游要现场检索再总结。
const SEARCH_TIMEOUT_SECS: f64 = 300.0;

/// `POST /v1/search` 的请求体。
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    /// 查询语句。
    pub prompt: String,
    /// 对外模型名。
    #[serde(default)]
    pub model: Option<String>,
}

/// `POST /v1/search`
pub async fn search(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SearchRequest>,
) -> Result<Response, ApiError> {
    let prompt = request.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "prompt 不能为空",
            "prompt",
        )));
    }
    let model = resolve_model(request.model.as_deref());
    let deadline = Deadline::in_secs(SEARCH_TIMEOUT_SECS);

    let lease = state
        .pool
        .acquire(&SelectionFilter::for_text(), deadline)
        .await
        .map_err(|error| match error {
            PoolError::NoCandidate { .. } => ApiError::openai(AppError::ServiceUnavailable {
                message: "无可用账号，请先通过 POST /api/accounts 导入".to_string(),
                code: "no_available_account".to_string(),
            }),
            PoolError::DeadlineExceeded => ApiError::openai(AppError::ServiceUnavailable {
                message: "等待账号槽位超时，请稍后重试".to_string(),
                code: "account_pool_timeout".to_string(),
            }),
            other => ApiError::openai(AppError::Internal(anyhow::anyhow!("{other}"))),
        })?;
    let account_id = lease.id();
    crate::call_log::note_account(account_id);
    let token = lease.access_token().to_string();

    let client = state.client_for(lease.account().proxy.trim()).map_err(|error| {
        ApiError::openai(AppError::Internal(anyhow::anyhow!("代理客户端构建失败：{error}")))
    })?;
    let outcome = gpt2apirs_generate::search::search(&client, &token, &prompt, &model, deadline).await;
    let succeeded = outcome.is_ok();
    state.pool.mark_text_result(account_id, succeeded).await.ok();
    drop(lease);

    let outcome = outcome.map_err(|error| {
        tracing::warn!(
            account = %account_id,
            kind = ?error.kind,
            message = %error.message,
            "搜索失败"
        );
        // 生成错误 → 对外错误只在这一处映射（与生图共用）
        crate::routes::image_flow::slot_error(error)
    })?;

    tracing::info!(
        account = %account_id,
        model = %model,
        conversation_id = %outcome.conversation_id,
        sources = outcome.sources.len(),
        "搜索完成"
    );

    Ok(Json(json!({
        "conversation_id": outcome.conversation_id,
        "model": model,
        "answer": outcome.answer,
        "status": outcome.status,
        "sources": outcome
            .sources
            .iter()
            .map(|source| json!({
                "title": source.title,
                "url": source.url,
                "snippet": source.snippet,
            }))
            .collect::<Vec<_>>(),
    }))
    .into_response())
}

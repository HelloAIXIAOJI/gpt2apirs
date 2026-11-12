//! 三个文本端点（`/v1/chat/completions`、`/v1/responses`、`/v1/messages`）的共用流程。

use std::sync::Arc;

use gpt2apirs_account::{PoolError, SelectionFilter};
use gpt2apirs_core::{AppError, Deadline};
use gpt2apirs_generate::FlowDiagnostics;
use gpt2apirs_generate::models;
use gpt2apirs_generate::text::{TextRequest, TextResult, generate_text};

use crate::error::ApiError;
use crate::state::AppState;

/// 读取富文本处理策略。
fn rich_text_policy(state: &AppState) -> crate::routes::rich_text::RichTextPolicy {
    crate::routes::rich_text::RichTextPolicy::from_setting(
        &state.settings.get_string("rich_text_policy"),
    )
}

/// 一次文本生成的产物（已记账、槽位已归还）。
pub struct TextOutcome {
    /// 是否命中缓存（命中时没有消耗上游配额，也没有实际账号）。
    pub cached: bool,
    /// 生成结果。
    pub result: TextResult,
    /// 实际服务的账号。
    pub account_id: uuid::Uuid,
    /// 过程诊断。
    pub diag: FlowDiagnostics,
    /// 归一的模型名。
    pub model: String,
}

/// 取号  生成  记账  归还。
pub async fn run_text(
    state: &Arc<AppState>,
    messages: &[(String, String)],
    requested_model: &str,
    deadline: Deadline,
) -> Result<TextOutcome, ApiError> {
    // ---- 富文本策略 ----
    let policy = rich_text_policy(state);
    let mut effective: Vec<(String, String)> = messages.to_vec();
    if policy.guard() {
        crate::routes::rich_text::apply_guard(&mut effective);
    }
    let messages = &effective[..];

    // ---- 缓存（只对非流式生效；流式不走这里）----
    let ttl = std::time::Duration::from_secs(
        state.settings.get_i64("chat_cache_ttl_secs").max(0) as u64,
    );
    let key = crate::routes::text_cache::cache_key(requested_model, messages);
    if let Some(cached) = crate::routes::text_cache::get(key, ttl) {
        tracing::info!(model = %requested_model, "命中文本缓存");
        return Ok(TextOutcome {
            cached: true,
            result: cached,
            account_id: uuid::Uuid::nil(),
            diag: FlowDiagnostics::default(),
            model: requested_model.to_string(),
        });
    }

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

    let mut request = TextRequest::new(String::new()).with_messages(messages.to_vec());
    request.model = requested_model.to_string();
    request.upstream_model = models::upstream_model_for(requested_model).to_string();

    let client = state.client_for(lease.account().proxy.trim()).map_err(|error| {
        ApiError::openai(AppError::Internal(anyhow::anyhow!("代理客户端构建失败：{error}")))
    })?;
    let outcome = generate_text(&client, lease.access_token(), &request, deadline).await;
    let succeeded = outcome.is_ok();
    // 文本记账不扣图片配额：两套额度互相独立
    let _ = state.pool.mark_text_result(account_id, succeeded).await;
    drop(lease);

    let (result, diag) = outcome.map_err(|error| {
        tracing::warn!(account = %account_id, %error, "文本生成失败");
        ApiError::openai(AppError::Upstream {
            message: error.to_string(),
            code: "upstream_error".to_string(),
            retryable: true,
            status: 502,
        })
    })?;

    // 输出侧：剥掉模型坚持要发的自闭合富文本组件
    let mut result = result;
    if policy.strip() {
        let stripped = crate::routes::rich_text::strip_rich_text(&result.text);
        if stripped.removed > 0 {
            tracing::warn!(
                removed = stripped.removed,
                before = result.text.chars().count(),
                after = stripped.text.chars().count(),
                "剥离富文本组件（上游仍在输出 UI 标签）"
            );
            result.text = stripped.text;
        }
    }

    // 观测：窄剥离之后还有标签 = guard 没压住，必须能被看见
    let residual = crate::routes::rich_text::count_residual_tags(&result.text);
    if residual > 0 {
        tracing::warn!(
            residual,
            model = %requested_model,
            "guard 未完全生效：输出里仍有组件标签（窄剥离按设计不处理小写 JSX）"
        );
    }

    // 只缓存成功且非空的回复
    crate::routes::text_cache::put(key, &result, ttl);

    Ok(TextOutcome {
        cached: false,
        result,
        account_id,
        diag,
        model: requested_model.to_string(),
    })
}

/// 构造「不支持工具」的错误。
///
/// 明确拒绝而不是塞一段「你不能用工具」的提示词：后者的效果取决于模型是否听话，
/// 调用方会以为工具真的被调用了。
///
/// 返回 [`AppError`] 而不是 [`ApiError`]：**错误格式必须由端点决定**。
/// `/v1/messages` 的客户端只会解析 Anthropic 格式 `{"type":"error","error":{...}}`，
/// 给它 OpenAI 格式等于让它在错误处理上再失败一次。
pub fn tools_not_supported(kind: &str) -> AppError {
    AppError::InvalidRequest {
        message: format!(
            "本实现不支持工具调用（{kind}）；如需图片请改用 POST /v1/images/generations"
        ),
        param: Some("tools".to_string()),
        code: Some("tools_not_supported".to_string()),
    }
}

/// 请求里是否**真的**带了工具。
///
/// `tools: []` 是很多客户端的默认值，不能当成「要调用工具」而拒绝 ——
/// 那会把正常请求挡在门外。只有非空数组（或非数组的怪异值）才算。
pub fn has_tools(tools: &Option<serde_json::Value>) -> bool {
    match tools {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    }
}

/// 算 (prompt, completion) token 数。三个文本端点共用同一份口径。
pub fn token_counts(
    messages: &[(String, String)],
    reply: &str,
    model: &str,
) -> (i64, i64) {
    let prompt = gpt2apirs_core::tokens::count_message_text_tokens(messages, model);
    let completion = gpt2apirs_core::tokens::count_text_tokens(reply, model);
    (prompt, completion)
}

/// 归一化对外模型名（空则用默认）。
pub fn resolve_model(requested: Option<&str>) -> String {
    requested
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("gpt-5-5")
        .to_string()
}

// 流式：边收边转发

/// 一次流式生成的控制块。
pub struct StreamHandle {
    /// 增量文本流（已附加「结束后记账」）。
    pub deltas: futures_util::stream::BoxStream<
        'static,
        std::result::Result<String, gpt2apirs_generate::error::GenerateError>,
    >,
    /// 结束后读全文。
    pub snapshot: std::sync::Arc<std::sync::Mutex<gpt2apirs_generate::text::StreamSnapshot>>,
    /// 归一后的模型名。
    pub model: String,
    /// 归一后的消息（算 prompt token 用）。
    pub messages: Vec<(String, String)>,
    /// 服务的账号。
    pub account_id: uuid::Uuid,
}

/// 开始一次流式生成。
///
/// **槽位与记账挂在流上**：流没读完之前不归还槽位、不记成功，
/// 否则「同时并发数」会在客户端还在收流时就被放开，等于限流失效。
pub async fn start_text_stream(
    state: &Arc<AppState>,
    messages: Vec<(String, String)>,
    requested_model: &str,
    deadline: Deadline,
) -> Result<StreamHandle, ApiError> {
    use futures_util::StreamExt;

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

    // ---- 富文本策略（流式同样注入 guard，输出侧用增量剥离器）----
    let policy = rich_text_policy(state);
    let mut effective = messages.clone();
    if policy.guard() {
        crate::routes::rich_text::apply_guard(&mut effective);
    }

    let mut request = TextRequest::new(String::new()).with_messages(effective.clone());
    request.model = requested_model.to_string();
    request.upstream_model = models::upstream_model_for(requested_model).to_string();

    let stream_client = state.client_for(lease.account().proxy.trim()).map_err(|error| {
        ApiError::openai(AppError::Internal(anyhow::anyhow!("代理客户端构建失败：{error}")))
    })?;
    let stream = gpt2apirs_generate::text::generate_text_stream(
        stream_client.as_ref(),
        lease.access_token(),
        &request,
        deadline,
    )
    .await
    .map_err(|error| {
        tracing::warn!(account = %account_id, %error, "文本流式生成失败");
        ApiError::openai(AppError::Upstream {
            message: error.to_string(),
            code: error.kind.code().to_string(),
            retryable: error.kind.retryable(),
            status: 502,
        })
    })?;

    let snapshot = stream.snapshot.clone();
    let pool = state.pool.clone();
    let mut inner = stream.deltas;

    // 输出侧增量剥离：组件可能跨 chunk，先用剥离器压住可能的标签开头
    if policy.strip() {
        use futures_util::StreamExt;
        inner = inner
            .scan(
                crate::routes::rich_text::StreamStripper::new(),
                |stripper, item| {
                    let output = match &item {
                        Ok(delta) => {
                            let ready = stripper.push(delta);
                            if ready.is_empty() {
                                None
                            } else {
                                Some(Ok(ready))
                            }
                        }
                        Err(_) => Some(item.clone()),
                    };
                    let removed = stripper.removed();
                    if removed > 0 {
                        tracing::debug!(removed, "流式剥离富文本组件");
                    }
                    futures_util::future::ready(output)
                },
            )
            .boxed();
    }

    // 状态：内层流 + 槽位 + 是否已记账
    struct State {
        inner: futures_util::stream::BoxStream<
            'static,
            std::result::Result<String, gpt2apirs_generate::error::GenerateError>,
        >,
        pool: std::sync::Arc<gpt2apirs_account::AccountPool>,
        lease: Option<gpt2apirs_account::AccountLease>,
        account_id: uuid::Uuid,
        snapshot: std::sync::Arc<std::sync::Mutex<gpt2apirs_generate::text::StreamSnapshot>>,
        settled: bool,
        streamed_any: bool,
    }

    let state_inner = State {
        inner: std::mem::replace(&mut inner, futures_util::stream::empty().boxed()),
        pool,
        lease: Some(lease),
        account_id,
        snapshot,
        settled: false,
        streamed_any: false,
    };

    let deltas = futures_util::stream::unfold(state_inner, |mut st| async move {
        if let Some(item) = st.inner.next().await {
            if item.is_ok() {
                st.streamed_any = true;
            }
            return Some((item, st));
        }
        // 流结束：记账 + 归还槽位（只做一次）
        if !st.settled {
            st.settled = true;
            let success = st.streamed_any
                && st
                    .snapshot
                    .lock()
                    .map(|guard| !guard.text.trim().is_empty())
                    .unwrap_or(false);
            // 文本不扣图片配额
            st.pool.mark_text_result(st.account_id, success).await.ok();
            st.lease.take();
        }
        None
    })
    .boxed();

    Ok(StreamHandle {
        deltas,
        snapshot: stream.snapshot,
        model: requested_model.to_string(),
        messages: effective,
        account_id,
    })
}

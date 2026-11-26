//! 三个文本端点（`/v1/chat/completions`、`/v1/responses`、`/v1/messages`）的共用流程。

use std::collections::HashSet;
use std::sync::Arc;

use gpt2apirs_account::{AccountLease, PoolError, SelectionFilter};
use gpt2apirs_generate::error::GenerateError;
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

/// 拆成「实际发给上游的消息」与「计费口径的消息」。
///
/// guard 提示词是**我们**为了压住富文本组件才加的，不该算进用户账单：
/// 曾经流式路径把注入后的列表当计费口径，导致同一请求流式报 149、非流式报 77。
/// 两条清单必须同时产出，调用方拿错一条就是扣费错误。
pub fn dispatch_messages(
    policy: crate::routes::rich_text::RichTextPolicy,
    messages: Vec<(String, String)>,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let billing = messages.clone();
    let mut dispatch = messages;
    if policy.guard() {
        crate::routes::rich_text::apply_guard(&mut dispatch);
    }
    (dispatch, billing)
}

/// 把工具说明作为一条独立的 system 消息放在最前。
///
/// 为什么不拼进 user 消息：实测拼进 user 时模型把它当成「用户的话」，
/// 而网页版模型自带联网，会直接自己查完就回答，连 `tool_choice: required` 都不理会。
/// 独立 system 消息的权重不同，是目前试下来唯一可能压过内置能力的写法。
fn inject_tools(messages: &mut Vec<(String, String)>, prompt: &str) {
    if prompt.trim().is_empty() {
        return;
    }
    messages.insert(0, ("system".to_string(), prompt.to_string()));
}

/// 预填给模型的 assistant 开头。
///
/// 实测：只在提示词里要求「输出这个 XML」没用 —— 上游模型自带联网，
/// 会自己查完就回答（连 `tool_choice: required` 都不理会）。
/// 把它已经「开口」的内容塞进上下文，续写才肯照着格式走。


/// 池层失败转对外错误。
///
/// 「池里没号」必须和「上游失败」分开：前者是运维该去导号，后者是上游问题。
fn pool_error(error: PoolError) -> ApiError {
    match error {
        PoolError::NoCandidate { .. } => ApiError::openai(AppError::ServiceUnavailable {
            message: "无可用账号，请先通过 POST /api/accounts 导入".to_string(),
            code: "no_available_account".to_string(),
        }),
        PoolError::DeadlineExceeded => ApiError::openai(AppError::ServiceUnavailable {
            message: "等待账号槽位超时，请稍后重试".to_string(),
            code: "account_pool_timeout".to_string(),
        }),
        other => ApiError::openai(AppError::Internal(anyhow::anyhow!("{other}"))),
    }
}

/// 上游失败转对外错误。
///
/// 分类由 generate 层给出；拿不到分类说明错误在传输层，按可重试处理。
fn upstream_error(error: Option<anyhow::Error>) -> ApiError {
    let message = error
        .as_ref()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "上游未返回结果".to_string());
    let classified = error
        .as_ref()
        .and_then(|value| value.downcast_ref::<GenerateError>());
    ApiError::openai(AppError::Upstream {
        message,
        code: classified
            .map(|failure| failure.kind.code().to_string())
            .unwrap_or_else(|| "upstream_error".to_string()),
        retryable: classified.map(|failure| failure.kind.retryable()).unwrap_or(true),
        // 状态码由分类决定：请求级问题必须回 400，否则调用方会当服务端故障去重试
        status: classified.map(|failure| failure.kind.status()).unwrap_or(502),
    })
}

fn attempts_limit(state: &AppState) -> usize {
    state
        .settings
        .get_i64("text_max_account_attempts")
        .max(1) as usize
}

/// 取号 + 执行，失败按分类换号重试。
///
/// 与生图那条 `image_flow::with_retry` 是同一套判断（`switch_account` 决定要不要换），
/// 差别只有两处：筛选用文本用途（允许 limited 账号）、记账走 `mark_text_result`（不扣图片配额）。
/// **成功不在这里记账**：流式要等流结束才算成功，由调用方决定时机。
async fn with_text_retry<T, F, Fut>(
    state: &Arc<AppState>,
    deadline: Deadline,
    mut op: F,
) -> Result<(T, uuid::Uuid, AccountLease), ApiError>
where
    F: FnMut(Arc<gpt2apirs_http::Client>, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let max_attempts = attempts_limit(state);
    let mut excluded: HashSet<uuid::Uuid> = HashSet::new();
    let mut last_error: Option<anyhow::Error> = None;

    for _ in 0..max_attempts {
        let filter = SelectionFilter {
            exclude: excluded.clone(),
            ..SelectionFilter::for_text()
        };
        let lease = match state.pool.acquire(&filter, deadline).await {
            Ok(lease) => lease,
            // 排除掉失败账号后没号了：把上一次的失败原因带出去
            Err(PoolError::NoCandidate { .. }) if !excluded.is_empty() => break,
            Err(error) => return Err(pool_error(error)),
        };
        let account_id = lease.id();
        crate::call_log::note_account(account_id);

        // 出口统一走全局
        let client = state
            .client_for("")
            .map_err(|error| {
                ApiError::openai(AppError::Internal(anyhow::anyhow!(
                    "代理客户端构建失败：{error}"
                )))
            })?;
        let token = lease.access_token().to_string();

        match op(client, token).await {
            Ok(value) => return Ok((value, account_id, lease)),
            Err(error) => {
                let _ = state.pool.mark_text_result(account_id, false).await;
                let classified = error.downcast_ref::<GenerateError>();
                if let Some(failure) = classified {
                    if failure.kind.blames_account() {
                        state
                            .pool
                            .handle_invalid(account_id, &failure.message)
                            .await
                            .ok();
                    }
                    if !failure.kind.switch_account() {
                        // 内容策略 / 验证码：换号没用，立刻停
                        last_error = Some(error);
                        break;
                    }
                }
                tracing::warn!(account = %account_id, %error, "文本生成失败，换号重试");
                excluded.insert(account_id);
                last_error = Some(error);
            }
        }
        drop(lease);
    }

    Err(upstream_error(last_error))
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

/// 一次图文对话里带的图片来源（尚未上传）。
pub type ImageSources = Vec<gpt2apirs_generate::inputs::ImageSource>;

/// 把图片来源取成字节并上传，得到能放进 payload 的引用。
///
/// 上传必须发生在拿到账号 token 之后，所以在换号重试的闭包里调用 ——
/// 换号后图片会重新上传一次，这一点和生图链路一致。
async fn upload_image_sources(
    client: &gpt2apirs_http::Client,
    token: &str,
    user_agent: &str,
    sources: &[gpt2apirs_generate::inputs::ImageSource],
    deadline: Deadline,
) -> anyhow::Result<Vec<gpt2apirs_protocol::conversation::ImageReference>> {
    use gpt2apirs_generate::inputs::resolve_sources;
    use gpt2apirs_generate::upload::upload_image;

    let resolved = resolve_sources(client, sources, deadline)
        .await
        .map_err(|error| anyhow::anyhow!("图片解析失败：{error}"))?;
    let mut references = Vec::with_capacity(resolved.len());
    for image in resolved {
        let uploaded = upload_image(
            client,
            token,
            user_agent,
            &image.data,
            Some(image.filename.as_str()),
            deadline,
        )
        .await?;
        references.push(uploaded.as_reference());
    }
    Ok(references)
}

/// 取号  生成  记账  归还。
pub async fn run_text(
    state: &Arc<AppState>,
    messages: &[(String, String)],
    images: &[gpt2apirs_generate::inputs::ImageSource],
    requested_model: &str,
    deadline: Deadline,
    thinking_effort: Option<&str>,
    // 工具说明（`text_tool` 才有）：拼进上游消息，但不计入计费口径。
    tool_prompt: Option<&str>,
) -> Result<TextOutcome, ApiError> {
    // ---- 富文本策略：只改发给上游的内容；usage 由端点用原始消息单独算 ----
    let policy = rich_text_policy(state);
    let (dispatch, _billing) = dispatch_messages(policy, messages.to_vec());
    let mut dispatch = dispatch;
    if let Some(prompt) = tool_prompt {
        inject_tools(&mut dispatch, prompt);
    }
    let messages = &dispatch[..];

    // ---- 缓存（只对非流式生效；流式不走这里）----
    let ttl = std::time::Duration::from_secs(
        state.settings.get_i64("chat_cache_ttl_secs").max(0) as u64,
    );
    // 带图时不走缓存：缓存键只看文本，命中会把「图里是什么」答成上一次的纯文本结果
    let key = crate::routes::text_cache::cache_key(requested_model, thinking_effort, messages);
    // 带工具时不缓存：缓存键只有模型 + 消息，同一句话配不同工具会串；
    // 而且工具调用是这一轮的指令，不该被上一次的结果顶掉。
    if tool_prompt.is_none()
        && images.is_empty()
        && let Some(cached) = crate::routes::text_cache::get(key, ttl)
    {
        tracing::info!(model = %requested_model, "命中文本缓存");
        return Ok(TextOutcome {
            cached: true,
            result: cached,
            account_id: uuid::Uuid::nil(),
            diag: FlowDiagnostics::default(),
            model: requested_model.to_string(),
        });
    }

    let mut request = TextRequest::new(String::new())
        .with_messages(messages.to_vec())
        .with_thinking_effort(thinking_effort.map(str::to_string));
    request.model = requested_model.to_string();
    request.upstream_model = models::upstream_model_for(requested_model).to_string();

    // 换号重试：401/限流/配额这类换号有意义的失败会换下一个账号
    let ((mut result, diag), account_id, lease) = with_text_retry(state, deadline, |client, token| {
        let mut request = request.clone();
        let sources = images.to_vec();
        async move {
            if !sources.is_empty() {
                request.images = upload_image_sources(
                    client.as_ref(),
                    &token,
                    &request.user_agent,
                    &sources,
                    deadline,
                )
                .await?;
            }
            generate_text(client.as_ref(), &token, &request, deadline).await
        }
    })
    .await?;
    // 文本记账不扣图片配额：两套额度互相独立
    let _ = state.pool.mark_text_result(account_id, true).await;
    drop(lease);


    // 输出侧：剥掉模型坚持要发的自闭合富文本组件
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

    // 只缓存成功且非空的回复（带工具的不进缓存，见上面的 tool_prompt 判断）
    if tool_prompt.is_none() {
        crate::routes::text_cache::put(key, &result, ttl);
    }

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
        .unwrap_or(gpt2apirs_generate::models::DEFAULT_TEXT_MODEL)
        .to_string()
}

// 流式：边收边转发

/// 一次流式生成的控制块。
pub struct StreamHandle {
    /// 增量流（正文与思考内容分开；已附加「结束后记账」）。
    pub deltas: futures_util::stream::BoxStream<
        'static,
        std::result::Result<
            gpt2apirs_protocol::text_stream::Delta,
            gpt2apirs_generate::error::GenerateError,
        >,
    >,
    /// 结束后读全文。
    pub snapshot: std::sync::Arc<std::sync::Mutex<gpt2apirs_generate::text::StreamSnapshot>>,
    /// 归一后的模型名。
    pub model: String,
    /// **计费口径**的消息：原始消息，不含内部注入的 guard 提示词。
    ///
    /// 别改成发给上游的那份 —— 那会把我们的提示词算进用户账单。
    pub billing_messages: Vec<(String, String)>,
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
    images: Vec<gpt2apirs_generate::inputs::ImageSource>,
    requested_model: &str,
    deadline: Deadline,
    thinking_effort: Option<&str>,
    // 工具说明（`text_tool` 才有）。
    tool_prompt: Option<&str>,
) -> Result<StreamHandle, ApiError> {
    use futures_util::StreamExt;

    // ---- 富文本策略（流式同样注入 guard，输出侧用增量剥离器）----
    let policy = rich_text_policy(state);
    let (dispatch, billing) = dispatch_messages(policy, messages);
    let mut dispatch = dispatch;
    if let Some(prompt) = tool_prompt {
        inject_tools(&mut dispatch, prompt);
    }

    let mut request = TextRequest::new(String::new())
        .with_messages(dispatch)
        .with_thinking_effort(thinking_effort.map(str::to_string));
    request.model = requested_model.to_string();
    request.upstream_model = models::upstream_model_for(requested_model).to_string();

    // 起流之前失败还能换号；流开始之后就不再重试（部分内容已经吐出去了）
    let (stream, account_id, lease) = with_text_retry(state, deadline, |client, token| {
        let mut request = request.clone();
        let sources = images.clone();
        async move {
            if !sources.is_empty() {
                request.images = upload_image_sources(
                    client.as_ref(),
                    &token,
                    &request.user_agent,
                    &sources,
                    deadline,
                )
                .await?;
            }
            gpt2apirs_generate::text::generate_text_stream(
                client.as_ref(),
                &token,
                &request,
                deadline,
            )
            .await
            .map_err(anyhow::Error::from)
        }
    })
    .await?;

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
                        Ok(gpt2apirs_protocol::text_stream::Delta::Text(text)) => {
                            let ready = stripper.push(text);
                            if ready.is_empty() {
                                None
                            } else {
                                Some(Ok(gpt2apirs_protocol::text_stream::Delta::Text(ready)))
                            }
                        }
                        // 思考内容不做剥离：它不是给用户看的富文本
                        Ok(gpt2apirs_protocol::text_stream::Delta::Reasoning(_)) => {
                            Some(item.clone())
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
            std::result::Result<
                gpt2apirs_protocol::text_stream::Delta,
                gpt2apirs_generate::error::GenerateError,
            >,
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
        billing_messages: billing,
        account_id,
    })
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::routes::rich_text::{RichTextPolicy, GUARD_MESSAGE};

    /// guard 提示词只进「发给上游」的那份，不进「计费口径」那份。
    ///
    /// 这条测试曾经能抓住真 bug：流式端点拿错清单，同一个请求报 149 而非 77。
    #[test]
    fn guard_goes_to_dispatch_but_not_to_billing() {
        let raw = vec![("user".to_string(), "你好".to_string())];
        let (dispatch, billing) = dispatch_messages(RichTextPolicy::GuardStrip, raw.clone());

        assert_eq!(dispatch.len(), raw.len() + 1, "guard 应插进派发消息");
        assert!(dispatch[0].1.contains(GUARD_MESSAGE), "派发消息首位应是 guard");
        assert_eq!(billing, raw, "计费口径必须保持原样");
        assert!(
            !billing.iter().any(|(_, text)| text.contains(GUARD_MESSAGE)),
            "计费消息里不该有 guard：{billing:?}"
        );
    }

    /// 策略关闭时不注入，两张清单完全一致。
    #[test]
    fn policy_off_keeps_both_lists_identical() {
        let raw = vec![("user".to_string(), "hi".to_string())];
        let (dispatch, billing) = dispatch_messages(RichTextPolicy::Off, raw.clone());
        assert_eq!(dispatch, raw);
        assert_eq!(billing, raw);
    }

    /// 计费口径的 token 数不受 guard 影响 —— 与 /v1/messages/count_tokens 同一个答案。
    #[test]
    fn billing_tokens_do_not_include_guard() {
        let raw = vec![("user".to_string(), "hello".to_string())];
        let (dispatch, billing) = dispatch_messages(RichTextPolicy::GuardStrip, raw.clone());
        let billing_tokens = gpt2apirs_core::tokens::count_message_text_tokens(&billing, gpt2apirs_generate::models::DEFAULT_MODEL);
        let dispatch_tokens =
            gpt2apirs_core::tokens::count_message_text_tokens(&dispatch, gpt2apirs_generate::models::DEFAULT_MODEL);

        assert_eq!(billing_tokens, 8, "hello 的计费口径固定为 8");
        assert!(
            dispatch_tokens > billing_tokens,
            "guard 会抬高派发侧 token（{dispatch_tokens} vs {billing_tokens}）"
        );
    }
}

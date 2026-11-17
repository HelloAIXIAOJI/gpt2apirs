//! 图片生成流水线：把 protocol 与 http 两个 crate 串起来。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod conversation;
pub mod error;
pub mod format;
pub mod inputs;
pub mod mask;
pub mod search;
pub mod text;
pub mod models;
pub mod upload;

use anyhow::{Context, Result};

use crate::error::{FailureKind, GenerateError, classify_http};
use chrono::Utc;
use gpt2apirs_core::paths;
use gpt2apirs_core::Deadline;
use gpt2apirs_http::{Client, browser_headers};
use gpt2apirs_protocol::conversation::{
    DEFAULT_MODEL, build_conversation_payload, build_conversation_prepare_payload,
    parse_conduit_token,
};
use gpt2apirs_protocol::download::{
    attachment_download_path, file_download_path, is_same_origin, parse_download_url,
    plan_download_targets,
};
use gpt2apirs_protocol::endpoints::{
    CONVERSATION, CONVERSATION_PREPARE, HOME, SENTINEL_FINALIZE, SENTINEL_PREPARE, base_url,
    url as upstream_url,
};
use gpt2apirs_protocol::pow::{build_legacy_requirements_token, build_proof_token};
use gpt2apirs_protocol::pow_config::build_pow_config;
use gpt2apirs_protocol::pow_config::parse_pow_resources;
use gpt2apirs_protocol::sentinel::{
    build_finalize_payload, build_prepare_payload, parse_finalize, parse_prepare,
};
use gpt2apirs_protocol::sse::{extract_conversation_ids, parse_conversation_stream};
use serde_json::Value;

/// 各阶段的**上限**（实际取 min(上限, 剩余总时间)）。
pub mod stage_caps {
    use std::time::Duration;
    /// bootstrap 取首页。
    pub const BOOTSTRAP: Duration = Duration::from_secs(30);
    /// sentinel prepare/finalize。
    pub const SENTINEL: Duration = Duration::from_secs(30);
    /// conversation/prepare。
    pub const CONVERSATION_PREPARE: Duration = Duration::from_secs(60);
    /// SSE 流。
    pub const SSE: Duration = Duration::from_secs(80);
    /// 解析下载地址。
    pub const RESOLVE_URL: Duration = Duration::from_secs(60);
    /// 下载图片。
    pub const DOWNLOAD: Duration = Duration::from_secs(120);
}

/// 一次请求的总时限：`stream(80) + poll(60) + 30 = 170s`。
pub fn default_request_deadline(settings: &gpt2apirs_core::RuntimeSettings) -> Deadline {
    let stream = settings.get_i64("image_stream_timeout_secs").max(1);
    let poll = settings.get_i64("image_poll_timeout_secs").max(1);
    Deadline::in_secs((stream + poll + 30) as f64)
}

/// 默认 User-Agent（指纹与 PoW 配置构造都依赖它，不要随意改）。
pub use gpt2apirs_http::DEFAULT_USER_AGENT;

/// 结果轮询策略。
///
/// 上游有时 SSE 已经结束、图片 id 却还没进会话文档：这时立刻去查会拿到空结果，
/// 甚至在流关闭后的一两百毫秒内吃到 429。所以先等一轮，再按间隔查，总预算封顶。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollPolicy {
    /// 首次查询前的等待（秒）。
    pub initial_wait_secs: f64,
    /// 两次查询的间隔（秒）。
    pub interval_secs: f64,
    /// 轮询总预算（秒）。
    pub timeout_secs: f64,
}

impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            initial_wait_secs: 5.0,
            interval_secs: 5.0,
            timeout_secs: 60.0,
        }
    }
}

impl PollPolicy {
    /// 从运行层设置读。
    ///
    /// 间隔下限 0.5s：再小只是把上游打爆，不会更快拿到图。
    pub fn from_settings(settings: &gpt2apirs_core::RuntimeSettings) -> Self {
        Self {
            initial_wait_secs: settings
                .get_f64("image_poll_initial_wait_secs")
                .clamp(0.0, 60.0),
            interval_secs: settings
                .get_f64("image_poll_interval_secs")
                .clamp(0.5, 60.0),
            timeout_secs: settings.get_i64("image_poll_timeout_secs").max(1) as f64,
        }
    }

    /// 预算比一个间隔还短就没有轮询的意义。
    pub fn is_enabled(&self) -> bool {
        self.timeout_secs > 0.0 && self.timeout_secs >= self.interval_secs
    }
}

/// 生成请求。
#[derive(Debug, Clone)]
pub struct ImageRequest {
    /// 提示词。
    pub prompt: String,
    /// 对外模型名（透传记录用）。
    pub model: String,
    /// 上游模型 slug。
    pub upstream_model: String,
    /// User-Agent。
    pub user_agent: String,
    /// 生成张数（1..=4）。
    pub n: u32,
    /// 输出格式。`None` 表示调用方没指定 —— 此时不发格式指令，行为与旧版一致。
    pub output_format: Option<crate::format::OutputFormat>,
    /// 动图帧数（仅 GIF 有意义；不填就不给上游）。
    pub frame_count: Option<u32>,
    /// 动图每帧时长（毫秒，仅 GIF 有意义）。
    pub frame_duration_ms: Option<u32>,
    /// 目标质量（`low`/`medium`/`high`，已校验）。`None` 表示不干预。
    ///
    /// **上游没有质量参数**：只拼进提示词当倾向；token 估算另按此值算。
    pub quality: Option<String>,
    /// 目标尺寸（`宽x高`，已校验）。`None` 表示不干预。
    ///
    /// **上游没有尺寸参数**：这里只拼进提示词当倾向，不保证精确。
    pub size: Option<String>,
    /// 参考图（**已上传到上游**）。非空即图生图。
    ///
    /// 上传必须在拿到账号 token 之后做，所以由调用方先上传再填这里 ——
    /// generate 不负责上传，它只管「怎么用」。
    pub images: Vec<gpt2apirs_protocol::conversation::ImageReference>,
    /// 结果轮询策略（SSE 没带回图片时的兜底）。
    pub poll: PollPolicy,
}

impl ImageRequest {
    /// 用提示词构造默认请求。
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: "gpt-image-2".to_string(),
            upstream_model: DEFAULT_MODEL.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            n: 1,
            quality: None,
            size: None,
            output_format: None,
            frame_count: None,
            frame_duration_ms: None,
            images: Vec::new(),
            poll: PollPolicy::default(),
        }
    }

    /// 带上参考图（图生图）。
    pub fn with_images(
        mut self,
        images: Vec<gpt2apirs_protocol::conversation::ImageReference>,
    ) -> Self {
        self.images = images;
        self
    }

    /// 真正发给上游的提示词：原文 + 格式/帧/尺寸指令。
    ///
    /// 这是提示词的**唯一装配点**：新增参数要影响上游，就加在这里，别在调用方另拼。
    pub fn effective_prompt(&self) -> String {
        let hints = crate::format::prompt_hints(
            self.output_format,
            self.frame_count,
            self.frame_duration_ms,
            self.size.as_deref(),
            self.quality.as_deref(),
        );
        crate::format::with_prompt_hints(&self.prompt, &hints)
    }

    /// 带上目标质量（调用方保证已校验）。
    pub fn with_quality(mut self, quality: Option<String>) -> Self {
        self.quality = quality;
        self
    }

    /// 带上目标尺寸（调用方保证已校验成 `宽x高`）。
    pub fn with_size(mut self, size: Option<String>) -> Self {
        self.size = size;
        self
    }

    /// 指定输出格式。
    pub fn with_output_format(mut self, format: crate::format::OutputFormat) -> Self {
        self.output_format = Some(format);
        self
    }

    /// 是否图生图。
    pub fn is_edit(&self) -> bool {
        !self.images.is_empty()
    }

    /// 设置张数（自动钳到 1..=4）。
    pub fn with_n(mut self, n: u32) -> Self {
        self.n = n.clamp(1, 32);
        self
    }
}

/// 已落盘的一张图片。
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    /// 图片字节。
    pub bytes: Vec<u8>,
    /// 相对 `DATA_DIR` 的路径，例如 `images/2026/11/11/<uuid>.png`。
    pub rel_path: String,
    /// MIME 类型，例如 `image/png`。
    pub mime: &'static str,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

impl GeneratedImage {
    /// base64 内容（`response_format=b64_json` 用）。
    pub fn to_base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
}

/// 生成结果。
#[derive(Debug, Clone, Default)]
pub struct ImageResult {
    /// 生成的图片。
    pub images: Vec<GeneratedImage>,
    /// 对话 ID。
    pub conversation_id: String,
    /// 解析出的 sediment ID。
    pub sediment_ids: Vec<String>,
    /// 解析出的 file ID。
    pub file_ids: Vec<String>,
    /// 原始 SSE（排错用）。
    pub sse_raw: String,
}

/// 各阶段诊断信息。
#[derive(Debug, Clone, Default)]
pub struct FlowDiagnostics {
    /// bootstrap 状态码。
    pub bootstrap_status: u16,
    /// HTML 长度。
    pub html_len: usize,
    /// 脚本数量。
    pub script_count: usize,
    /// data-build。
    pub data_build: String,
    /// prepare 状态码。
    pub prepare_status: u16,
    /// PoW 是否被要求。
    pub pow_required: bool,
    /// PoW 难度。
    pub pow_difficulty: String,
    /// Turnstile 是否被要求。
    pub turnstile_required: bool,
    /// 上游给的 Turnstile 字节码长度（0 表示没给）。
    pub turnstile_dx_len: usize,
    /// 本次送进 finalize 的 Turnstile token 长度（0 表示空串）。
    pub turnstile_token_len: usize,
    /// finalize 状态码。
    pub finalize_status: u16,
    /// conversation/prepare 状态码。
    pub conv_prepare_status: u16,
    /// SSE 状态码。
    pub sse_status: u16,
    /// SSE 字节数。
    pub sse_bytes: usize,
    /// 轮询会话详情的次数（0 表示没进入兜底）。
    pub poll_attempts: u32,
    /// 轮询花了多少秒。
    pub poll_seconds: f64,
}

/// 执行完整生图流水线（`n` 张）。
pub async fn generate_images(
    client: &Client,
    token: &str,
    request: &ImageRequest,
    deadline: Deadline,
) -> std::result::Result<(ImageResult, FlowDiagnostics), GenerateError> {
    if token.trim().is_empty() {
        return Err(GenerateError::new(
            FailureKind::TokenInvalid,
            "access_token 为空",
        ));
    }
    let mut combined = ImageResult::default();
    let mut diagnostics = FlowDiagnostics::default();

    for index in 0..request.n.clamp(1, 32) {
        let (result, diag) = generate_one(client, token, request, deadline).await?;
        if index == 0 {
            diagnostics = diag;
        }
        combined.conversation_id = result.conversation_id;
        combined.sediment_ids.extend(result.sediment_ids);
        combined.file_ids.extend(result.file_ids);
        combined.sse_raw = result.sse_raw;
        combined.images.extend(result.images);
    }

    Ok((combined, diagnostics))
}

/// 生成一张。
/// 一次握手的产物。
pub struct Handshake {
    /// sentinel 最终确认拿到的 requirements token。
    pub requirements_token: String,
    /// PoW 证明 token（没有 PoW 时为空串）。
    pub proof_token: String,
    /// 过程诊断。
    pub diag: FlowDiagnostics,
}

/// 执行握手：bootstrap  sentinel prepare  PoW  sentinel finalize。
///
/// 图片与文本链路共用这一段，所以抽出来避免两份实现漂移。
pub async fn handshake(
    client: &Client,
    token: &str,
    user_agent: &str,
    deadline: Deadline,
) -> std::result::Result<Handshake, GenerateError> {
    let mut diag = FlowDiagnostics::default();
    let mut headers = browser_headers(user_agent);
    headers.push((
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_string(),
    ));

    // ---- 1. bootstrap ----
    let budget = stage_budget(deadline, "bootstrap", stage_caps::BOOTSTRAP)?;
    let home = client
        .get_text(&upstream_url(HOME), &headers, Some(budget))
        .await?;
    diag.bootstrap_status = home.status;
    diag.html_len = home.body.len();
    if home.status != 200 {
        return Err(GenerateError::new(
            classify_http(home.status, &home.body),
            format!("bootstrap 失败，状态 {}", home.status),
        ));
    }
    let (scripts, data_build) = parse_pow_resources(&home.body);
    diag.script_count = scripts.len();
    diag.data_build = data_build.clone();

    // ---- 2. sentinel prepare ----
    let config = build_pow_config(user_agent, Some(&scripts), &data_build);
    let p_token = build_legacy_requirements_token(&config);

    let budget = stage_budget(deadline, "sentinel_prepare", stage_caps::SENTINEL)?;
    let prepare = client
        .post_json(
            &upstream_url(SENTINEL_PREPARE),
            &gpt2apirs_http::api_headers(user_agent, token, SENTINEL_PREPARE),
            &build_prepare_payload(&p_token),
            Some(budget),
        )
        .await?;
    diag.prepare_status = prepare.status;
    if prepare.status != 200 {
        return Err(GenerateError::new(
            classify_http(prepare.status, &prepare.body),
            format!("sentinel prepare 失败，状态 {}: {}", prepare.status, clip(&prepare.body)),
        ));
    }
    let prepare_json: Value =
        serde_json::from_str(&prepare.body).context("prepare 响应不是 JSON")?;
    let prepared = parse_prepare(&prepare_json);

    if prepared.arkose_required {
        return Err(GenerateError::new(
            FailureKind::ArkoseRequired,
            "上游要求 arkose 验证码，本项目未实现",
        ));
    }
    diag.pow_required = prepared.pow.is_some();
    diag.turnstile_required = prepared.turnstile_required;
    diag.turnstile_dx_len = prepared
        .turnstile_dx
        .as_deref()
        .map(str::len)
        .unwrap_or(0);

    // Turnstile 求解：拿到 dx 才算得出来，算不出来就发空串（与旧行为一致）
    let turnstile_token = match (prepared.turnstile_required, prepared.turnstile_dx.as_deref()) {
        (true, Some(dx)) => {
            // 排错/写 fixture 用：只有显式设了目录才落盘，平时不在磁盘留上游挑战
            if let Ok(dir) = std::env::var("GPT2APIRS_CAPTURE_TURNSTILE") {
                if !dir.trim().is_empty() {
                    match gpt2apirs_protocol::turnstile::capture(
                        std::path::Path::new(dir.trim()),
                        dx,
                        &p_token,
                    ) {
                        Ok(path) => tracing::info!(target = %path.display(), "已落盘 Turnstile 挑战"),
                        Err(error) => tracing::warn!(%error, "落盘 Turnstile 挑战失败"),
                    }
                }
            }
            let solved = gpt2apirs_protocol::turnstile::solve_challenge(dx, &p_token);
            if solved.is_none() {
                tracing::warn!(
                    dx_len = dx.len(),
                    "Turnstile 求解失败，按空 token 继续（上游当前不强制校验）"
                );
            }
            solved.unwrap_or_default()
        }
        _ => String::new(),
    };
    diag.turnstile_token_len = turnstile_token.len();

    let proof_token = match &prepared.pow {
        Some(challenge) if challenge.is_usable() => {
            diag.pow_difficulty = challenge.difficulty.clone();
            build_proof_token(&challenge.seed, &challenge.difficulty, &config)
                .context("PoW 求解失败")?
        }
        _ => String::new(),
    };

    // ---- 3. sentinel finalize ----
    let budget = stage_budget(deadline, "sentinel_finalize", stage_caps::SENTINEL)?;
    let finalize = client
        .post_json(
            &upstream_url(SENTINEL_FINALIZE),
            &gpt2apirs_http::api_headers(user_agent, token, SENTINEL_FINALIZE),
            &build_finalize_payload(&prepared.prepare_token, &proof_token, &turnstile_token),
            Some(budget),
        )
        .await?;
    diag.finalize_status = finalize.status;
    let finalize_json: Value =
        serde_json::from_str(&finalize.body).context("finalize 响应不是 JSON")?;
    let finalized = parse_finalize(&finalize_json);
    if finalized.token.is_empty() {
        return Err(GenerateError::new(
            FailureKind::Protocol,
            format!("finalize 未返回 token: {}", clip(&finalize.body)),
        ));
    }

    Ok(Handshake {
        requirements_token: finalized.token,
        proof_token,
        diag,
    })
}

async fn generate_one(
    client: &Client,
    token: &str,
    request: &ImageRequest,
    deadline: Deadline,
) -> std::result::Result<(ImageResult, FlowDiagnostics), GenerateError> {
    // ---- 1~3. bootstrap + sentinel ----
    let Handshake {
        requirements_token,
        proof_token,
        mut diag,
    } = handshake(client, token, &request.user_agent, deadline).await?;
    let auth = format!("Bearer {token}");

    // ---- 4. conversation/prepare ----
    let budget = stage_budget(deadline, "conversation_prepare", stage_caps::CONVERSATION_PREPARE)?;
    let prep = client
        .post_json(
            &upstream_url(CONVERSATION_PREPARE),
            &image_headers(
                &request.user_agent,
                token,
                CONVERSATION_PREPARE,
                &requirements_token,
                &proof_token,
                None,
                "application/json",
            ),
            &build_conversation_prepare_payload(&request.effective_prompt(), &request.upstream_model),
            Some(budget),
        )
        .await?;
    diag.conv_prepare_status = prep.status;
    if prep.status != 200 {
        return Err(GenerateError::new(
            classify_http(prep.status, &prep.body),
            format!(
                "conversation/prepare 失败，状态 {}: {}",
                prep.status,
                clip(&prep.body)
            ),
        ));
    }    let prep_json: Value = serde_json::from_str(&prep.body).context("prepare 响应不是 JSON")?;
    let Some(conduit_token) = parse_conduit_token(&prep_json) else {
        return Err(GenerateError::new(
            FailureKind::Protocol,
            format!(
                "conversation/prepare 未返回 conduit_token: {}",
                clip(&prep.body)
            ),
        ));
    };

    // ---- 5. SSE ----
    let budget = stage_budget(deadline, "sse", stage_caps::SSE)?;
    let sse = client
        .post_sse(
            &upstream_url(CONVERSATION),
            &image_headers(
                &request.user_agent,
                token,
                CONVERSATION,
                &requirements_token,
                &proof_token,
                Some(&conduit_token),
                "text/event-stream",
            ),
            &build_conversation_payload(
                &request.effective_prompt(),
                &request.upstream_model,
                gpt2apirs_protocol::util::now_unix_secs_f64(),
                &request.images,
            ),
            Some(budget),
        )
        .await?;
    diag.sse_status = sse.status;
    diag.sse_bytes = sse.body.len();
    if sse.status != 200 {
        return Err(GenerateError::new(
            classify_http(sse.status, &sse.body),
            format!("生图 SSE 请求失败，状态 {}: {}", sse.status, clip(&sse.body)),
        ));
    }

    let (_, state) = parse_conversation_stream(&sse.body);
    let mut result = ImageResult {
        conversation_id: state.conversation_id.clone(),
        sediment_ids: state.sediment_ids.clone(),
        file_ids: state.file_ids.clone(),
        sse_raw: sse.body.clone(),
        images: Vec::new(),
    };

    // ---- 6/7. 解析下载地址 + 下载 + 落盘 ----
    let target_format = request.output_format.unwrap_or_default();
    fetch_images(
        client,
        &auth,
        request,
        &state.conversation_id,
        &state.file_ids,
        &state.sediment_ids,
        target_format,
        deadline,
        &mut result.images,
    )
    .await?;

    // ---- 8. 兜底：SSE 结束却没图时轮询会话详情，等后到的图片 id ----
    if result.images.is_empty() && request.poll.is_enabled() {
        let started = std::time::Instant::now();
        poll_for_images(
            client, token, &auth, request, target_format, deadline, &mut result, &mut diag,
        )
        .await?;
        diag.poll_seconds = started.elapsed().as_secs_f64();
    }

    Ok((result, diag))
}

/// 解析下载地址并下载，成功的图片追加到 `images`。
///
/// 同一 URL 只下一次：sediment 与 file 两条路径可能指向同一份内容。
#[allow(clippy::too_many_arguments)]
async fn fetch_images(
    client: &Client,
    auth: &str,
    request: &ImageRequest,
    conversation_id: &str,
    file_ids: &[String],
    sediment_ids: &[String],
    target_format: crate::format::OutputFormat,
    deadline: Deadline,
    images: &mut Vec<GeneratedImage>,
) -> std::result::Result<(), GenerateError> {
    let (attachments, files) = plan_download_targets(conversation_id, file_ids, sediment_ids);

    let mut urls: Vec<String> = Vec::new();
    for (conv_id, sediment_id) in &attachments {
        let path = attachment_download_path(conv_id, sediment_id);
        if let Ok(Some(url)) = resolve_url(client, auth, &request.user_agent, &path, deadline).await {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    for file_id in &files {
        let path = file_download_path(file_id);
        if let Ok(Some(url)) = resolve_url(client, auth, &request.user_agent, &path, deadline).await {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }

    download_urls(client, auth, &request.user_agent, &urls, target_format, deadline, images).await
}

/// 逐个下载 URL，成功的落盘后追加到 `images`。
///
/// 单张下载失败只跳过，不影响其它张（与旧行为一致）。
#[allow(clippy::too_many_arguments)]
async fn download_urls(
    client: &Client,
    auth: &str,
    user_agent: &str,
    urls: &[String],
    target_format: crate::format::OutputFormat,
    deadline: Deadline,
    images: &mut Vec<GeneratedImage>,
) -> std::result::Result<(), GenerateError> {
    for url in urls {
        let mut headers = vec![("User-Agent", user_agent.to_string())];
        if is_same_origin(base_url(), url) {
            headers.push(("Authorization", auth.to_string()));
        }
        let Ok(budget) = stage_budget(deadline, "download", stage_caps::DOWNLOAD) else {
            continue;
        };
        let Ok(response) = client.get_bytes(url, &headers, Some(budget)).await else {
            continue;
        };
        if response.status != 200 {
            continue;
        }
        match save_image_as(&response.body, target_format) {
            Ok(Some(saved)) => images.push(saved),
            Ok(None) => {}
            Err(error) => {
                // 图已经生成、配额已经花了：落盘失败不能把它丢掉，
                // 也不能让整个请求失败。记一条 WARN，返回一份「未落盘」的图片，
                // 由调用方决定只回 b64、不给 URL。
                tracing::warn!(%error, "图片落盘失败，本次只返回字节");
                if let Some(unpersisted) = unpersisted_image(&response.body, target_format) {
                    images.push(unpersisted);
                }
            }
        }
    }
    Ok(())
}

/// 轮询会话详情补齐后到的图片 id。
///
/// SSE 结束时图可能还没写进会话文档：先按 `initial_wait` 等一轮，再按 `interval` 重查，
/// 遇到暂时性错误就翻倍退避（封顶 16s）。总时长同时受轮询预算与请求总时限约束，
/// 一旦下到图或出现新 id 就重试下载，拿到图立即返回。
#[allow(clippy::too_many_arguments)]
async fn poll_for_images(
    client: &Client,
    token: &str,
    auth: &str,
    request: &ImageRequest,
    target_format: crate::format::OutputFormat,
    deadline: Deadline,
    result: &mut ImageResult,
    diag: &mut FlowDiagnostics,
) -> std::result::Result<(), GenerateError> {
    let policy = request.poll;
    let conversation_id = result.conversation_id.clone();
    if conversation_id.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let cap = policy.interval_secs.max(16.0);
    let mut wait = if result.file_ids.is_empty() && result.sediment_ids.is_empty() {
        policy.initial_wait_secs
    } else {
        policy.interval_secs
    };

    loop {
        if started.elapsed().as_secs_f64() >= policy.timeout_secs || deadline.is_expired() {
            break;
        }
        let budget_left = policy.timeout_secs - started.elapsed().as_secs_f64();
        let deadline_left = deadline.remaining().map(|d| d.as_secs_f64()).unwrap_or(budget_left);
        let sleep = wait.min(budget_left).min(deadline_left);
        if sleep < 0.0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs_f64(sleep)).await;

        diag.poll_attempts += 1;
        let payload = match conversation::fetch_conversation_payload(
            client,
            token,
            &request.user_agent,
            &conversation_id,
            deadline,
        )
        .await
        {
            Ok(payload) => payload,
            Err(_) => {
                wait = (wait.max(0.5) * 2.0).min(cap);
                continue;
            }
        };
        let (_, file_ids, sediment_ids) = extract_conversation_ids(&payload.to_string());
        let grew = merge_ids(&mut result.file_ids, file_ids)
            | merge_ids(&mut result.sediment_ids, sediment_ids);
        if grew {
            fetch_images(
                client,
                auth,
                request,
                &conversation_id,
                &result.file_ids,
                &result.sediment_ids,
                target_format,
                deadline,
                &mut result.images,
            )
            .await?;
            if !result.images.is_empty() {
                break;
            }
        }
        wait = (wait.max(0.5) * 2.0).min(cap);
    }
    Ok(())
}

/// 把 `incoming` 合并进 `existing`，返回是否有新增。
fn merge_ids(existing: &mut Vec<String>, incoming: Vec<String>) -> bool {
    let mut grew = false;
    for id in incoming {
        if !existing.contains(&id) {
            existing.push(id);
            grew = true;
        }
    }
    grew
}

/// 把图片字节落盘，返回落盘后的元信息。
///
/// 路径：`DATA_DIR/images/YYYY/MM/DD/<uuid>.<ext>`
///
/// 无法识别的格式返回 `Ok(None)`。
pub fn save_image(bytes: &[u8]) -> Result<Option<GeneratedImage>> {
    save_image_as(bytes, crate::format::OutputFormat::Png)
}

/// 按目标格式落盘：先转码，再用目标格式的扩展名与 MIME。
///
/// 已是目标格式时不做无谓重编码（见 `format::convert`）。
pub fn save_image_as(
    bytes: &[u8],
    format: crate::format::OutputFormat,
) -> Result<Option<GeneratedImage>> {
    if image::guess_format(bytes).is_err() {
        return Ok(None);
    }
    let converted = crate::format::convert(bytes, format)?;
    // 扩展名与 MIME 一律取**实际字节**：GIF 会被原样透传，
    // 这时若按请求格式写扩展名，就会得到「名叫 .png 的 GIF」。
    let Some((mime, ext)) = sniff_mime(&converted) else {
        return Ok(None);
    };
    let (width, height) = gpt2apirs_core::tokens::image_size_from_bytes(&converted).unwrap_or((0, 0));

    let date = Utc::now().format("%Y/%m/%d").to_string();
    let rel_path = format!("images/{date}/{}.{ext}", gpt2apirs_core::util::new_uuid());

    let root = paths::data_dir()?;
    let absolute = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建图片目录失败: {}", parent.display()))?;
    }
    // 先写临时文件再改名，避免半截文件
    let temp = absolute.with_extension(format!("{ext}.part"));
    std::fs::write(&temp, &converted)
        .with_context(|| format!("写入图片失败: {}", temp.display()))?;
    std::fs::rename(&temp, &absolute)
        .with_context(|| format!("重命名图片失败: {}", absolute.display()))?;

    Ok(Some(GeneratedImage {
        bytes: converted,
        rel_path,
        mime,
        width,
        height,
    }))
}

/// 落盘失败时的兜底：拿到格式与尺寸，但 `rel_path` 留空表示没有落地文件。
fn unpersisted_image(
    bytes: &[u8],
    format: crate::format::OutputFormat,
) -> Option<GeneratedImage> {
    let converted = crate::format::convert(bytes, format).ok()?;
    let (mime, _) = sniff_mime(&converted)?;
    let (width, height) =
        gpt2apirs_core::tokens::image_size_from_bytes(&converted).unwrap_or((0, 0));
    Some(GeneratedImage {
        bytes: converted,
        rel_path: String::new(),
        mime,
        width,
        height,
    })
}

/// 识别图片格式，返回 `(mime, 扩展名)`。
pub fn sniff_mime(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(("image/png", "png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    if bytes.starts_with(b"GIF8") {
        return Some(("image/gif", "gif"));
    }
    None
}


/// 图片元信息：MIME、扩展名、宽高。
///
/// 上游申请上传时要报 `width` / `height` / `file_size`，所以要真的解一次图头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageMeta {
    /// MIME。
    pub mime: &'static str,
    /// 扩展名（不含点）。
    pub ext: &'static str,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

/// 读取图片元信息。不是受支持的图片则返回 `None`。
///
/// 复用 `core::tokens::image_size_from_bytes`，不额外引 `image` 依赖。
pub fn image_meta(bytes: &[u8]) -> Option<ImageMeta> {
    let (mime, ext) = sniff_mime(bytes)?;
    let (width, height) = gpt2apirs_core::tokens::image_size_from_bytes(bytes)?;
    Some(ImageMeta {
        mime,
        ext,
        width,
        height,
    })
}

async fn resolve_url(
    client: &Client,
    auth: &str,
    user_agent: &str,
    path: &str,
    deadline: Deadline,
) -> Result<Option<String>> {
    let budget = stage_budget(deadline, "resolve_url", stage_caps::RESOLVE_URL)?;
    let headers = vec![
        ("User-Agent", user_agent.to_string()),
        ("Accept", "application/json".to_string()),
        ("Authorization", auth.to_string()),
        ("Origin", base_url().to_string()),
        ("Referer", format!("{}/", base_url())),
        ("X-OpenAI-Target-Path", path.to_string()),
        ("X-OpenAI-Target-Route", path.to_string()),
    ];
    let response = client
        .get_text(&upstream_url(path), &headers, Some(budget))
        .await?;
    if response.status != 200 {
        return Ok(None);
    }
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    Ok(parse_download_url(&body))
}

#[allow(clippy::too_many_arguments)]
fn image_headers(
    user_agent: &str,
    token: &str,
    path: &str,
    requirements_token: &str,
    proof_token: &str,
    conduit_token: Option<&str>,
    accept: &str,
) -> Vec<(&'static str, String)> {
    let mut headers = gpt2apirs_http::api_headers(user_agent, token, path);
    for entry in headers.iter_mut() {
        if entry.0 == "Accept" {
            entry.1 = accept.to_string();
        }
    }
    headers.push((
        "OpenAI-Sentinel-Chat-Requirements-Token",
        requirements_token.to_string(),
    ));
    if !proof_token.is_empty() {
        headers.push(("OpenAI-Sentinel-Proof-Token", proof_token.to_string()));
    }
    if let Some(conduit) = conduit_token {
        headers.push(("X-Conduit-Token", conduit.to_string()));
    }
    if accept == "text/event-stream" {
        headers.push((
            "X-Oai-Turn-Trace-Id",
            gpt2apirs_core::util::new_uuid(),
        ));
    }
    headers
}

/// 取阶段预算，超时则转成 `anyhow` 错误（带阶段名）。
pub(crate) fn stage_budget(
    deadline: Deadline,
    stage: &'static str,
    cap: std::time::Duration,
) -> Result<std::time::Duration> {
    deadline
        .budget(stage, cap)
        .map_err(|_| anyhow::anyhow!("请求总时限已耗尽（阶段 {stage}）"))
}

fn clip(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognises_formats() {
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).map(|(m, _)| m), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]).map(|(m, _)| m), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a....").map(|(m, _)| m), Some("image/gif"));
        assert_eq!(sniff_mime(b"not an image"), None);
    }

    #[test]
    fn unpersisted_image_keeps_bytes_without_path() {
        // 落盘失败时仍要保留图片：rel_path 为空即「没落地」。
        let mut buffer = Vec::new();
        let pixel = image::RgbaImage::from_pixel(2, 3, image::Rgba([200, 40, 40, 255]));
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(
                &mut std::io::Cursor::new(&mut buffer),
                image::ImageFormat::Png,
            )
            .expect("编码 PNG");
        let image = unpersisted_image(&buffer, crate::format::OutputFormat::Png)
            .expect("应能构造未落盘对象");
        assert!(image.rel_path.is_empty(), "未落盘不应有路径");
        assert_eq!(image.width, 2);
        assert_eq!(image.height, 3);
        assert!(!image.bytes.is_empty());
    }

    #[test]
    fn save_image_rejects_non_image() {
        let saved = save_image(b"hello").expect("不应报错");
        assert!(saved.is_none(), "非图片应返回 None");
    }

    #[test]
    fn request_n_is_clamped() {
        // 真实上限由 API 层按设置项 `image_max_n` 校验（≤32）；
        // 这里只是执行层的安全网，防止调用方绕过校验塞进天文数字。
        assert_eq!(ImageRequest::new("x").with_n(0).n, 1);
        assert_eq!(ImageRequest::new("x").with_n(99).n, 32);
        assert_eq!(ImageRequest::new("x").with_n(8).n, 8);
        assert_eq!(ImageRequest::new("x").with_n(3).n, 3);
    }

    #[test]
    fn prompt_not_mutated_without_format() {
        // 关键回归：没指定输出格式时，提示词必须与旧版完全一致
        let request = ImageRequest::new("画一只猫");
        assert_eq!(request.effective_prompt(), "画一只猫");
    }

    #[test]
    fn prompt_gets_format_hint_when_requested() {
        let request = ImageRequest::new("画一只猫")
            .with_output_format(crate::format::OutputFormat::Jpeg);
        let prompt = request.effective_prompt();
        assert!(prompt.starts_with("画一只猫"));
        assert!(prompt.contains("生成格式为 jpeg。"), "实际: {prompt}");
    }

    #[test]
    fn gif_hint_carries_frame_options() {
        let mut request = ImageRequest::new("旋转立方体")
            .with_output_format(crate::format::OutputFormat::Gif);
        request.frame_count = Some(20);
        request.frame_duration_ms = Some(120);
        let prompt = request.effective_prompt();
        assert!(prompt.contains("生成格式为 gif。"));
        assert!(prompt.contains("frame_count: 20"));
        assert!(prompt.contains("frame_duration_ms: 120"));
    }

    #[test]
    fn poll_policy_default_is_enabled() {
        let policy = PollPolicy::default();
        assert!(policy.is_enabled());
        assert_eq!(policy.initial_wait_secs, 5.0);
        assert_eq!(policy.interval_secs, 5.0);
    }

    #[test]
    fn poll_policy_reads_settings_and_clamps() {
        use serde_json::json;
        let mut values = std::collections::HashMap::new();
        values.insert("image_poll_initial_wait_secs".to_string(), json!(0));
        values.insert("image_poll_interval_secs".to_string(), json!(0.1));
        values.insert("image_poll_timeout_secs".to_string(), json!(30));
        let settings = gpt2apirs_core::RuntimeSettings::new(values);
        let policy = PollPolicy::from_settings(&settings);
        assert_eq!(policy.initial_wait_secs, 0.0);
        // 间隔下限 0.5s
        assert_eq!(policy.interval_secs, 0.5);
        assert_eq!(policy.timeout_secs, 30.0);
        assert!(policy.is_enabled());
    }

    #[test]
    fn poll_policy_disabled_when_budget_below_interval() {
        let policy = PollPolicy { initial_wait_secs: 5.0, interval_secs: 10.0, timeout_secs: 5.0 };
        assert!(!policy.is_enabled());
    }

    #[test]
    fn merge_ids_appends_only_new() {
        let mut existing = vec!["a".to_string(), "b".to_string()];
        let grew = merge_ids(&mut existing, vec!["b".to_string(), "c".to_string()]);
        assert!(grew);
        assert_eq!(existing, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        let grew_again = merge_ids(&mut existing, vec!["a".to_string()]);
        assert!(!grew_again);
        assert_eq!(existing.len(), 3);
    }
}

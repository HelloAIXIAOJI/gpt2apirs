//! `POST /v1/images/generations` —— OpenAI 兼容文生图。

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::extract::{Request, State};
use gpt2apirs_core::AppError;
use gpt2apirs_generate::{ImageRequest, default_request_deadline};
use serde::Deserialize;

use crate::error::ApiError;
use crate::routes::image_flow::run_generation;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
/// OpenAI 兼容的生成请求。
pub struct ImageGenerationRequest {
    /// 提示词（必填，非空）。
    pub prompt: String,
    /// 对外模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// 生成张数（1..=4）。
    #[serde(default)]
    pub n: Option<u32>,
    /// 尺寸，如 `1024x1024`。
    #[serde(default)]
    pub size: Option<String>,
    /// 质量：`auto` / `low` / `high`。
    #[serde(default)]
    pub quality: Option<String>,
    /// 输出图片格式：`png`（默认）/ `jpeg` / `webp` / `gif`。
    #[serde(default)]
    pub output_format: Option<String>,
    /// 动图帧数（仅 `output_format=gif` 有意义）。
    #[serde(default)]
    pub frame_count: Option<u32>,
    /// 动图每帧时长（毫秒，仅 `output_format=gif` 有意义）。
    #[serde(default)]
    pub frame_duration_ms: Option<u32>,
    /// 返回格式：`b64_json`（默认）或 `url`。
    #[serde(default)]
    pub response_format: Option<String>,
    /// 是否流式（M4 暂不支持）。
    #[serde(default)]
    pub stream: Option<bool>,
}


/// `POST /v1/images/generations`
pub async fn generate(
    identity: Option<axum::Extension<crate::auth::ApiKeyIdentity>>,
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<axum::response::Response, ApiError> {
    // 申请 body：先取 Host 用于拼 URL，再读 JSON（顺序不能反）
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let scheme = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
        .to_string();

    let body: ImageGenerationRequest = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|err| {
            ApiError::openai(AppError::invalid(format!("读取请求体失败: {err}")))
        })
        .and_then(|bytes| {
            serde_json::from_slice(&bytes).map_err(|err| {
                ApiError::openai(
                    AppError::InvalidRequest {
                        message: format!("请求体不是合法 JSON: {err}"),
                        param: None,
                        code: Some("invalid_json".to_string()),
                    },
                )
            })
        })?;

    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "prompt 不能为空",
            "prompt",
        )));
    }

    let n = body.n.unwrap_or(1);
    let limit = crate::routes::image_common::max_n(&state.settings);
    if n == 0 || n > limit {
        return Err(ApiError::openai(AppError::invalid_param(
            format!("n 必须在 1..={limit}"),
            "n",
        )));
    }

    let response_format = body
        .response_format
        .clone()
        .unwrap_or_else(|| "b64_json".to_string());
    if response_format != "b64_json" && response_format != "url" {
        return Err(ApiError::openai(AppError::invalid_param(
            "response_format 只能是 b64_json 或 url",
            "response_format",
        )));
    }
    if body.stream == Some(true) {
        return Err(ApiError::openai(AppError::invalid_param(
            "上游不提供逐块图片流（SSE 只回 sediment:// 指针，字节要等生成完成后另行下载）；\
        要边等边查进度请用 POST /api/image-tasks 提交，再用 GET /api/image-tasks/{id} 轮询",
            "stream",
        )));
    }

    // 校验模型名（在取 token 之前，保证参数错误优先于配置错误返回）
    let requested_model = body
        .model
        .clone()
        .unwrap_or_else(|| gpt2apirs_generate::models::default_image_model().to_string());
    if !gpt2apirs_generate::models::is_supported_image(&requested_model) {
        let supported = gpt2apirs_generate::models::SUPPORTED_MODELS.join(", ");
        let mut message = format!("不支持的模型 {requested_model:?}；当前支持: {supported}");
        if gpt2apirs_generate::models::NOT_YET_SUPPORTED_MODELS
            .contains(&requested_model.as_str())
        {
            message.push_str("（该模型需要 Codex 链路，尚未实现）");
        }
        return Err(ApiError::openai(AppError::invalid_param(message, "model")));
    }

    let output_format = match body.output_format.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        None => None,
        Some(raw) => Some(gpt2apirs_generate::format::OutputFormat::parse(raw).ok_or_else(|| {
            ApiError::openai(AppError::invalid_param(
                "output_format 只能是 png / jpeg / webp / gif",
                "output_format",
            ))
        })?),
    };
    let frame_count = body.frame_count;
    let frame_duration_ms = body.frame_duration_ms;
    let size = crate::routes::image_common::checked_size(body.size.as_deref())?;
    let quality = crate::routes::image_common::checked_quality(body.quality.as_deref())?;

    let mut image_request = ImageRequest::new(prompt.clone()).with_n(n);
    // 只有要回 b64 时才保留字节；url 模式图已落盘，留着纯属白占内存。
    image_request.keep_bytes = response_format != "url";
    image_request.model = requested_model.clone();
    image_request.upstream_model =
        gpt2apirs_generate::models::upstream_model_for(&requested_model).to_string();
    image_request.output_format = output_format;
    image_request.frame_count = frame_count;
    image_request.frame_duration_ms = frame_duration_ms;
    image_request.size = size;
    image_request.quality = quality.clone();

    let quality = quality.unwrap_or_else(|| "auto".to_string());
    let base_url = state.public_base_url(&scheme, &host);

    let deadline = default_request_deadline(&state.settings);

    // 从账号池取一个账号；租约 Drop 时自动归还槽位。
    // ---- 并行 n 张 + 换号重试（见 image_flow）----
        crate::routes::image_flow::ensure_disk_space()?;
    let mut slots = run_generation(&state, image_request, deadline).await?;

    let key = identity.map(|axum::Extension(value)| value);
    // 资产表按全量记（多给的那张也进图库，数据不丢）……
    crate::routes::image_flow::record_assets(&state, &slots, &prompt, &[], key.as_ref()).await;
    // ……但响应严格守 n 的契约：n=1 就只回 1 张。
    let dropped = crate::routes::image_flow::clamp_images(&mut slots, n);
    if dropped > 0 {
        tracing::info!(
            requested = n,
            dropped,
            "上游多返回了图片，按 n 截断（多出的已进资产表）"
        );
    }

    let mut images: Vec<&gpt2apirs_generate::GeneratedImage> = Vec::new();
    for slot in &slots {
        for image in &slot.images {
            images.push(image);
        }
    }

    tracing::info!(
        slots = slots.len(),
        images = images.len(),
        accounts = slots.iter().map(|s| s.account_id.to_string()).collect::<Vec<_>>().join(","),
        "生图完成"
    );

    let mut output_tokens = 0i64;
    for image in &images {
        output_tokens += gpt2apirs_core::tokens::count_generated_image_tokens(
            image.width,
            image.height,
            &quality,
        );
    }
    let want_b64 = response_format != "url";
    // 借用到此为止：下面把字节从 slots 里搬进流式响应，避免整串 base64 复制。
    drop(images);
    let mut items = Vec::with_capacity(slots.iter().map(|slot| slot.images.len()).sum());
    for slot in &mut slots {
        for image in std::mem::take(&mut slot.images) {
            items.push(crate::image_response::StreamItem {
                source: if want_b64 {
                    crate::image_response::ItemSource::Bytes(image.bytes)
                } else {
                    crate::image_response::ItemSource::Url(if image.rel_path.is_empty() {
                        String::new()
                    } else {
                        format!("{base_url}/{}", image.rel_path)
                    })
                },
                prompt: Some(prompt.clone()),
            });
        }
    }

    // 上游只回了文字（反问、要参考图、解释）：照文本模型的做法把话返回给调用方，
    // 而不是报一个「没有图片」的错。
    let message = if items.is_empty() {
        slots
            .iter()
            .map(|slot| slot.text.trim())
            .find(|text| !text.is_empty())
            .map(str::to_string)
    } else {
        None
    };

    Ok(crate::image_response::ImageResponse {
        created: chrono::Utc::now().timestamp(),
        usage: gpt2apirs_core::tokens::image_usage(0, 0, output_tokens),
        items,
        message,
    }
    .into_response())
}

//! `POST /v1/images/generations` —— OpenAI 兼容文生图。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use gpt2apirs_core::AppError;
use gpt2apirs_generate::{ImageRequest, default_request_deadline};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::routes::image_flow::run_generation;
use crate::state::AppState;

/// 合法张数上限。
const MAX_N: u32 = 4;

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

#[derive(Debug, Serialize)]
/// 单张结果的 OpenAI 结构。
pub struct ImageDatum {
    /// base64 内容（`b64_json` 模式）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    /// 图片 URL（`url` 模式）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 修正后的提示词（本实现原样回传）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
/// OpenAI 兼容的生成响应。
pub struct ImageGenerationResponse {
    /// 生成时间戳（秒）。
    pub created: i64,
    /// 结果列表。
    pub data: Vec<ImageDatum>,
    /// token 用量。
    pub usage: serde_json::Value,
    /// 上游没出图时的文字说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `POST /v1/images/generations`
pub async fn generate(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<ImageGenerationResponse>, ApiError> {
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
    if n == 0 || n > MAX_N {
        return Err(ApiError::openai(AppError::invalid_param(
            format!("n 必须在 1..={MAX_N}"),
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

    let mut image_request = ImageRequest::new(prompt.clone()).with_n(n);
    image_request.model = requested_model.clone();
    image_request.upstream_model =
        gpt2apirs_generate::models::upstream_model_for(&requested_model).to_string();
    image_request.output_format = output_format;
    image_request.frame_count = frame_count;
    image_request.frame_duration_ms = frame_duration_ms;
    image_request.size = size;

    let quality = body.quality.clone().unwrap_or_else(|| "auto".to_string());
    let base_url = state.public_base_url(&scheme, &host);

    let deadline = default_request_deadline(&state.settings);

    // 从账号池取一个账号；租约 Drop 时自动归还槽位。
    // ---- 并行 n 张 + 换号重试（见 image_flow）----
    let slots = run_generation(&state, image_request, deadline).await?;

    let mut images: Vec<&gpt2apirs_generate::GeneratedImage> = Vec::new();
    for slot in &slots {
        for image in &slot.images {
            images.push(image);
        }
    }
    crate::routes::image_flow::record_assets(&state, &slots, &prompt).await;

    tracing::info!(
        slots = slots.len(),
        images = images.len(),
        accounts = slots.iter().map(|s| s.account_id.to_string()).collect::<Vec<_>>().join(","),
        "生图完成"
    );

    let mut data = Vec::with_capacity(images.len());
    let mut output_tokens = 0i64;
    for image in &images {
        output_tokens += gpt2apirs_core::tokens::count_generated_image_tokens(
            image.width,
            image.height,
            &quality,
        );
        if response_format == "url" {
            data.push(ImageDatum {
                b64_json: None,
                url: Some(format!("{base_url}/{}", image.rel_path)),
                revised_prompt: Some(prompt.clone()),
            });
        } else {
            data.push(ImageDatum {
                b64_json: Some(image.to_base64()),
                url: None,
                revised_prompt: Some(prompt.clone()),
            });
        }
    }

    Ok(Json(ImageGenerationResponse {
        created: chrono::Utc::now().timestamp(),
        data,
        usage: gpt2apirs_core::tokens::image_usage(0, 0, output_tokens),
        message: None,
    }))
}

//! `POST /v1/images/edits` —— OpenAI 兼容图生图。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::Request;
use gpt2apirs_core::AppError;
use gpt2apirs_generate::inputs::{self, ImageSource};
use gpt2apirs_generate::mask::composite_mask;
use gpt2apirs_generate::{ImageRequest, default_request_deadline};
use serde_json::Value;

use crate::error::ApiError;
use crate::routes::image_flow::run_edit;
use crate::routes::v1_images::{ImageDatum, ImageGenerationResponse};
use crate::state::AppState;

/// 图片字段名（multipart）。
const IMAGE_FIELDS: &[&str] = &[
    "image",
    "image[]",
    "images",
    "images[]",
    "image_url",
    "image_url[]",
];
/// mask 字段名（multipart）。
const MASK_FIELDS: &[&str] = &["mask", "mask[]"];
/// 标量字段名。
const SCALAR_FIELDS: &[&str] = &[
    "prompt",
    "model",
    "n",
    "size",
    "quality",
    "response_format",
    "output_format",
    "frame_count",
    "frame_duration_ms",
    "stream",
];

/// 解析后的编辑请求。
#[derive(Debug, Default)]
struct EditRequest {
    prompt: String,
    model: Option<String>,
    n: Option<u32>,
    size: Option<String>,
    quality: Option<String>,
    response_format: Option<String>,
    output_format: Option<String>,
    frame_count: Option<u32>,
    frame_duration_ms: Option<u32>,
    stream: Option<bool>,
    images: Vec<ImageSource>,
    masks: Vec<ImageSource>,
}

/// 解析并准备好一次图生图请求（同步接口与异步任务共用同一套规则）。
pub struct PreparedEdit {
    /// 提示词。
    pub prompt: String,
    /// 发给上游的请求。
    pub request: ImageRequest,
    /// 参考图字节（已合成 mask）。
    pub prepared: Vec<(Vec<u8>, String)>,
    /// 响应格式：`url` 或 `b64_json`。
    pub response_format: String,
    /// 对外基础地址。
    pub base_url: String,
    /// 参考图折算的输入 token。
    pub input_image_tokens: i64,
    /// 质量档位（算输出 token 用）。
    pub quality: String,
}

/// 解析请求、解析参考图并构建上游请求。
pub async fn prepare_edit(
    state: &Arc<AppState>,
    request: Request<axum::body::Body>,
) -> Result<PreparedEdit, ApiError> {
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
    let input_limit = limit_from_mb(state.settings.get_i64("image_input_max_mb"));
    let content_type = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    let parsed = if content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        parse_json_body(request).await?
    } else if content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        parse_multipart(request, input_limit).await?
    } else {
        return Err(ApiError::openai(AppError::invalid_param(
            format!(
                "不支持的 Content-Type {content_type:?}；请用 multipart/form-data 或 application/json"
            ),
            "content-type",
        )));
    };

    validate(&parsed, &state.settings)?;

    let prompt = parsed.prompt.trim().to_string();
    let n = parsed
        .n
        .unwrap_or(1)
        .clamp(1, crate::routes::image_common::max_n(&state.settings));
    let response_format = parsed
        .response_format
        .as_deref()
        .unwrap_or("b64_json")
        .to_string();
    let output_format = match parsed
        .output_format
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => None,
        Some(raw) => Some(gpt2apirs_generate::format::OutputFormat::parse(raw).ok_or_else(|| {
            ApiError::openai(AppError::invalid_param(
                "output_format 只能是 png / jpeg / webp / gif",
                "output_format",
            ))
        })?),
    };
    let frame_count = parsed.frame_count;
    let frame_duration_ms = parsed.frame_duration_ms;
    let size = crate::routes::image_common::checked_size(parsed.size.as_deref())?;
    let quality_option = crate::routes::image_common::checked_quality(parsed.quality.as_deref())?;
    let quality = quality_option.clone().unwrap_or_else(|| "auto".to_string());
    let requested_model = parsed
        .model
        .clone()
        .unwrap_or_else(|| gpt2apirs_generate::models::default_image_model().to_string());
    let base_url = state.public_base_url(&scheme, &host);
    let deadline = default_request_deadline(&state.settings);

    // ---- 准备参考图（mask 合成）----
    let outbound = state
        .client_for("")
        .map_err(|error| ApiError::openai(AppError::Internal(anyhow::anyhow!("{error}"))))?;
    let resolved_images = inputs::resolve_sources(&outbound, &parsed.images, deadline)
        .await
        .map_err(input_error)?;
    let resolved_masks = if parsed.masks.is_empty() {
        Vec::new()
    } else {
        inputs::resolve_sources(&outbound, &parsed.masks, deadline)
            .await
            .map_err(input_error)?
    };

    let mut prepared: Vec<(Vec<u8>, String)> = Vec::with_capacity(resolved_images.len());
    for (index, image) in resolved_images.iter().enumerate() {
        let bytes = if resolved_masks.is_empty() {
            image.data.clone()
        } else {
            // mask 少于图片时，最后一张 mask 复用到剩下的图片
            let mask = &resolved_masks[index.min(resolved_masks.len() - 1)];
            composite_mask(&image.data, &mask.data).map_err(|error| {
                ApiError::openai(AppError::invalid_param(
                    format!("mask 合成失败：{error}"),
                    "mask",
                ))
            })?
        };
        prepared.push((bytes, image.filename.clone()));
    }

    // 参考图逐次计算输入 token（与哪张图被选中无关）
    let input_image_tokens: i64 = prepared
        .iter()
        .filter_map(|(bytes, _)| gpt2apirs_core::tokens::image_size_from_bytes(bytes))
        .map(|(width, height)| {
            gpt2apirs_core::tokens::count_image_input_tokens(width, height, &requested_model, "auto")
        })
        .sum();

    // ---- 并行 n 张 + 换号重试（内部会重新上传参考图）----
    let mut image_request = ImageRequest::new(prompt.clone()).with_n(n);
    image_request.model = requested_model.clone();
    image_request.upstream_model =
        gpt2apirs_generate::models::upstream_model_for(&requested_model).to_string();
    image_request.output_format = output_format;
    image_request.frame_count = frame_count;
    image_request.frame_duration_ms = frame_duration_ms;
    image_request.size = size;
    image_request.quality = quality_option;

    Ok(PreparedEdit {
        prompt,
        request: image_request,
        prepared,
        response_format,
        base_url,
        input_image_tokens,
        quality,
    })
}

/// `POST /v1/images/edits`
pub async fn edit_images(
    identity: Option<axum::Extension<crate::auth::ApiKeyIdentity>>,
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Json<ImageGenerationResponse>, ApiError> {
    let prepared_edit = prepare_edit(&state, request).await?;
    let prompt = prepared_edit.prompt.clone();
    let response_format = prepared_edit.response_format.clone();
    let base_url = prepared_edit.base_url.clone();
    let quality = prepared_edit.quality.clone();
    let input_image_tokens = prepared_edit.input_image_tokens;

    crate::routes::image_flow::ensure_disk_space()?;
    let deadline = default_request_deadline(&state.settings);
    let requested_n = prepared_edit.request.n;
    let slots = run_edit(
        &state,
        prepared_edit.request,
        prepared_edit.prepared.clone(),
        deadline,
    )
    .await?;

    let mut images: Vec<&gpt2apirs_generate::GeneratedImage> = Vec::new();
    for slot in &slots {
        for image in &slot.images {
            images.push(image);
        }
    }
    if images.len() > requested_n as usize {
        tracing::info!(
            requested = requested_n,
            got = images.len(),
            "上游多返回了图片"
        );
    }
    // 生成成功才落盘参考图：失败时不在磁盘上留孤儿。
    let references = crate::references::persist_all(&prepared_edit.prepared);
    let key = identity.map(|axum::Extension(value)| value);
    crate::routes::image_flow::record_assets(&state, &slots, &prompt, &references, key.as_ref()).await;

    tracing::info!(
        slots = slots.len(),
        images = images.len(),
        references = prepared_edit.prepared.len(),
        "图生图完成"
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
                url: if image.rel_path.is_empty() {
                    None
                } else {
                    Some(format!("{base_url}/{}", image.rel_path))
                },
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
        usage: gpt2apirs_core::tokens::image_usage(0, input_image_tokens, output_tokens),
        data,
        message: None,
    }))
}

fn validate(
    parsed: &EditRequest,
    settings: &gpt2apirs_core::settings::RuntimeSettings,
) -> Result<(), ApiError> {
    if parsed.prompt.trim().is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "prompt 不能为空",
            "prompt",
        )));
    }
    if let Some(n) = parsed.n {
        let limit = crate::routes::image_common::max_n(settings);
        if n == 0 || n > limit {
            return Err(ApiError::openai(AppError::invalid_param(
                format!("n 必须在 1..={limit}"),
                "n",
            )));
        }
    }
    if let Some(format) = parsed.response_format.as_deref() {
        if format != "b64_json" && format != "url" {
            return Err(ApiError::openai(AppError::invalid_param(
                "response_format 只能是 b64_json 或 url",
                "response_format",
            )));
        }
    }
    if parsed.stream == Some(true) {
        return Err(ApiError::openai(AppError::invalid_param(
            "上游不提供逐块图片流（SSE 只回 sediment:// 指针，字节要等生成完成后另行下载）；要边等边查进度请用 POST /api/image-tasks 提交，再用 GET /api/image-tasks/{id} 轮询",
            "stream",
        )));
    }
    if parsed.images.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "必须提供 image 或 image_url",
            "image",
        )));
    }
    Ok(())
}

fn input_error(error: inputs::InputError) -> ApiError {
    let param = if matches!(error, inputs::InputError::FileIdUnsupported) {
        "image"
    } else {
        "image"
    };
    ApiError::openai(AppError::invalid_param(error.to_string(), param))
}

fn scalar_into(parsed: &mut EditRequest, key: &str, value: &str) {
    match key {
        "prompt" => parsed.prompt = value.to_string(),
        "model" => parsed.model = Some(value.to_string()),
        "n" => parsed.n = value.trim().parse().ok(),
        "size" => parsed.size = Some(value.to_string()),
        "quality" => parsed.quality = Some(value.to_string()),
        "response_format" => parsed.response_format = Some(value.to_string()),
        "output_format" => parsed.output_format = Some(value.to_string()),
        "frame_count" => parsed.frame_count = value.trim().parse().ok(),
        "frame_duration_ms" => parsed.frame_duration_ms = value.trim().parse().ok(),
        "stream" => {
            parsed.stream = Some(matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            ))
        }
        _ => {}
    }
}

fn source_from_text_field(value: &str) -> Result<Vec<ImageSource>, ApiError> {
    let trimmed = value.trim();
    // 有些客户端把 image_url 写成 JSON 片段，先试着按 JSON 解析
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
            return inputs::sources_from_json(&json).map_err(input_error);
        }
    }
    inputs::sources_from_text(trimmed).map_err(input_error)
}

async fn parse_multipart(
    request: Request<axum::body::Body>,
    input_limit: usize,
) -> Result<EditRequest, ApiError> {
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|error| {
            ApiError::openai(AppError::invalid_param(
                format!("multipart 解析失败：{error}"),
                "content-type",
            ))
        })?;
    let mut parsed = EditRequest::default();
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        ApiError::openai(AppError::invalid_param(
            format!("读取表单字段失败：{error}"),
            "content-type",
        ))
    })? {
        let name = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().map(|value| value.to_string());
        if IMAGE_FIELDS.contains(&name.as_str()) || MASK_FIELDS.contains(&name.as_str()) {
            // 边读边计长度：超限立即断开，不把整张图读进内存再报错。
            let mut stream = field;
            let bytes = read_limited(&mut stream, &name, input_limit).await?;
            // 有 filename 视为上传文件；否则当作文本（可能是 base64 或 URL）
            let sources = match filename {
                Some(name_on_disk) if !name_on_disk.trim().is_empty() => {
                    vec![ImageSource::Inline {
                        data: bytes.to_vec(),
                        filename: name_on_disk,
                        mime: "application/octet-stream".to_string(),
                    }]
                }
                _ => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    source_from_text_field(&text)?
                }
            };
            if MASK_FIELDS.contains(&name.as_str()) {
                parsed.masks.extend(sources);
            } else {
                parsed.images.extend(sources);
            }
            continue;
        }
        if SCALAR_FIELDS.contains(&name.as_str()) {
            let bytes = field.bytes().await.map_err(|error| {
                ApiError::openai(AppError::invalid_param(
                    format!("读取字段 {name} 失败：{error}"),
                    &name,
                ))
            })?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            scalar_into(&mut parsed, &name, &text);
        }
        // 其余字段忽略：OpenAI 客户端会塞 user 等字段
    }
    Ok(parsed)
}

/// 运行设置里的 MB 值换成字节，并夹到 1..=50。
fn limit_from_mb(mb: i64) -> usize {
    mb.clamp(1, 50) as usize * 1024 * 1024
}

/// 读取一个图片字段，超过 `limit` 字节立即拒绝。
async fn read_limited(
    field: &mut axum::extract::multipart::Field<'_>,
    name: &str,
    limit: usize,
) -> Result<axum::body::Bytes, ApiError> {
    let mut data = Vec::new();
    loop {
        let chunk = field.chunk().await.map_err(|error| {
            ApiError::openai(AppError::invalid_param(
                format!("读取字段 {name} 失败：{error}"),
                name,
            ))
        })?;
        let Some(chunk) = chunk else { break };
        if data.len() + chunk.len() > limit {
            return Err(ApiError::openai(AppError::PayloadTooLarge {
                message: format!(
                    "字段 {name} 超过上限 {} MB，请压缩后再上传",
                    limit / (1024 * 1024)
                ),
                param: Some(name.to_string()),
            }));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data.into())
}

async fn parse_json_body(
    request: Request<axum::body::Body>,
) -> Result<EditRequest, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|error| {
            ApiError::openai(AppError::invalid(format!("读取请求体失败：{error}")))
        })?;
    let body: Value = serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::openai(AppError::InvalidRequest {
            message: format!("请求体不是合法 JSON：{error}"),
            param: None,
            code: Some("invalid_json".to_string()),
        })
    })?;
    let object = body.as_object().ok_or_else(|| {
        ApiError::openai(AppError::invalid("JSON 请求体必须是对象"))
    })?;

    let mut parsed = EditRequest::default();
    for (key, value) in object {
        if let Some(text) = value.as_str() {
            if SCALAR_FIELDS.contains(&key.as_str()) {
                scalar_into(&mut parsed, key, text);
                continue;
            }
        }
        if IMAGE_FIELDS.contains(&key.as_str()) {
            parsed.images.extend(inputs::sources_from_json(value).map_err(input_error)?);
        } else if MASK_FIELDS.contains(&key.as_str()) {
            parsed.masks.extend(inputs::sources_from_json(value).map_err(input_error)?);
        }
    }
    Ok(parsed)
}

// 让 Multipart 的 FromRequest 在本模块可用
use axum::extract::FromRequest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_from_mb_clamps() {
        assert_eq!(limit_from_mb(10), 10 * 1024 * 1024);
        assert_eq!(limit_from_mb(0), 1024 * 1024, "0 夹到 1 MB");
        assert_eq!(limit_from_mb(999), 50 * 1024 * 1024, "超过上限夹到 50 MB");
    }
}

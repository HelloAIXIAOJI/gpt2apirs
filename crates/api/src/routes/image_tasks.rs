//! `/api/image-tasks` —— 异步生图任务的提交与查询。
//!
//! 提交立即返回任务 id，生成在后台跑；客户端按 id 轮询状态与结果。
//! 鉴权与 `/v1/*` 一致用 API key（这是给 API 调用方用的，不是后台管理面）。

use std::sync::Arc;

use axum::Json;
use axum::response::IntoResponse;
use axum::extract::{Path, Query, Request, State};
use base64::Engine as _;
use gpt2apirs_core::AppError;
use gpt2apirs_generate::ImageRequest;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::image_tasks::{ImageTask, TaskStatus};
use crate::state::AppState;

/// 合法张数上限（与同步接口一致）。
#[derive(Debug, Deserialize)]
/// 提交异步生图任务的请求体。
pub struct TaskSubmitRequest {
    /// 提示词（必填，非空）。
    pub prompt: String,
    /// 对外模型名。
    #[serde(default)]
    pub model: Option<String>,
    /// 生成张数（1..=4）。
    #[serde(default)]
    pub n: Option<u32>,
    /// 尺寸。
    #[serde(default)]
    pub size: Option<String>,
    /// 质量。
    #[serde(default)]
    pub quality: Option<String>,
    /// 输出图片格式：png / jpeg / webp / gif。
    #[serde(default)]
    pub output_format: Option<String>,
    /// GIF 帧数（仅 `output_format=gif` 时有意义）。
    #[serde(default)]
    pub frame_count: Option<u32>,
    /// GIF 每帧时长（毫秒）。
    #[serde(default)]
    pub frame_duration_ms: Option<u32>,
    /// 结果返回格式：url（默认）/ b64_json。
    #[serde(default)]
    pub response_format: Option<String>,
}

#[derive(Debug, Serialize)]
/// 结果单张（url 或 b64）。
pub struct TaskDatum {
    /// 图片 URL。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// base64 内容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
}

#[derive(Debug, Serialize)]
/// 任务视图。
pub struct TaskView {
    /// 任务 id。
    pub id: String,
    /// 状态。
    pub status: &'static str,
    /// 提示词。
    pub prompt: String,
    /// 模型。
    pub model: String,
    /// 张数。
    pub n: u32,
    /// 输出格式（png/jpeg/webp/gif）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// GIF 帧数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_count: Option<u32>,
    /// GIF 帧时长（毫秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_duration_ms: Option<u32>,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    pub updated_at: String,
    /// 完成时间（RFC3339）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// 失败原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 当前阶段。
    pub progress: String,
    /// 耗时（毫秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// token 用量。
    pub usage: serde_json::Value,
    /// 结果。
    pub data: Vec<TaskDatum>,
}

/// 读 `ids` 查询参数（逗号分隔）。
fn parse_ids(raw: &str) -> Vec<Uuid> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect()
}

fn encode_b64(rel_path: &str) -> Option<String> {
    let root = gpt2apirs_core::paths::data_dir().ok()?;
    let absolute = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let bytes = std::fs::read(absolute).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// 把任务转成对外的视图。
///
/// `want_b64=false` 时一律给 URL：列表页一次几百条任务，
/// 每条都读盘编码 base64 会瞬间吃掉上百 MB。
pub fn task_view(task: &ImageTask, want_b64: bool) -> TaskView {
    let want_b64 = want_b64 && task.response_format == "b64_json";
    let mut data = Vec::with_capacity(task.images.len());
    for image in &task.images {
        if want_b64 {
            if let Some(b64) = encode_b64(&image.rel_path) {
                data.push(TaskDatum { url: None, b64_json: Some(b64) });
                continue;
            }
        }
        data.push(TaskDatum {
            url: Some(format!("{}/{}", task.base_url, image.rel_path)),
            b64_json: None,
        });
    }
    TaskView {
        id: task.id.to_string(),
        status: task.status.as_str(),
        prompt: task.prompt.clone(),
        model: task.model.clone(),
        n: task.n,
        output_format: task.output_format.clone(),
        frame_count: task.frame_count,
        frame_duration_ms: task.frame_duration_ms,
        created_at: task.created_at.to_rfc3339(),
        updated_at: task.updated_at.to_rfc3339(),
        finished_at: task.finished_at.map(|t| t.to_rfc3339()),
        error: task.error.clone(),
        progress: task.progress.clone(),
        duration_ms: task.duration_ms,
        usage: serde_json::json!({
            "input_tokens": task.input_tokens,
            "output_tokens": task.output_tokens,
            "total_tokens": task.input_tokens + task.output_tokens,
        }),
        data,
    }
}

/// `POST /api/image-tasks/generations`
pub async fn submit(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<TaskView>, ApiError> {
    let key = request
        .extensions()
        .get::<crate::auth::ApiKeyIdentity>()
        .cloned();
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

    let body: TaskSubmitRequest = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|err| ApiError::openai(AppError::invalid(format!("读取请求体失败: {err}"))))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes).map_err(|err| {
                ApiError::openai(AppError::InvalidRequest {
                    message: format!("请求体不是合法 JSON: {err}"),
                    param: None,
                    code: Some("invalid_json".to_string()),
                })
            })
        })?;

    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param("prompt 不能为空", "prompt")));
    }
    let n = body.n.unwrap_or(1);
    let limit = crate::routes::image_common::max_n(&state.settings);
    if n == 0 || n > limit {
        return Err(ApiError::openai(AppError::invalid_param(
            format!("n 必须在 1..={limit}"),
            "n",
        )));
    }
    let response_format = body.response_format.clone().unwrap_or_else(|| "url".to_string());
    if response_format != "url" && response_format != "b64_json" {
        return Err(ApiError::openai(AppError::invalid_param(
            "response_format 只能是 url 或 b64_json",
            "response_format",
        )));
    }
    let requested_model = body
        .model
        .clone()
        .unwrap_or_else(|| gpt2apirs_generate::models::default_image_model().to_string());
    if !gpt2apirs_generate::models::is_supported_image(&requested_model) {
        let supported = gpt2apirs_generate::models::SUPPORTED_MODELS.join(", ");
        return Err(ApiError::openai(AppError::invalid_param(
            format!("不支持的模型 {requested_model:?}；当前支持: {supported}"),
            "model",
        )));
    }
    let output_format = match body.output_format.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None => None,
        Some(raw) => Some(gpt2apirs_generate::format::OutputFormat::parse(raw).ok_or_else(|| {
            ApiError::openai(AppError::invalid_param(
                "output_format 只能是 png / jpeg / webp / gif",
                "output_format",
            ))
        })?),
    };

    let mut image_request = ImageRequest::new(prompt.clone()).with_n(n);
    image_request.model = requested_model.clone();
    image_request.upstream_model =
        gpt2apirs_generate::models::upstream_model_for(&requested_model).to_string();
    image_request.output_format = output_format;
    // 帧参数只在 GIF 时生效，与同步接口保持一致。
    image_request.frame_count = body.frame_count;
    image_request.frame_duration_ms = body.frame_duration_ms;
    image_request.size = crate::routes::image_common::checked_size(body.size.as_deref())?;
    image_request.quality = crate::routes::image_common::checked_quality(body.quality.as_deref())?;

    let base_url = state.public_base_url(&scheme, &host);
    let now = chrono::Utc::now();
    let task = ImageTask {
        id: Uuid::new_v4(),
        status: TaskStatus::Queued,
        prompt: prompt.clone(),
        model: requested_model,
        n,
        response_format,
        output_format: image_request.output_format.map(|format| format.label().to_string()),
        frame_count: image_request.frame_count,
        frame_duration_ms: image_request.frame_duration_ms,
        base_url,
        created_at: now,
        updated_at: now,
        finished_at: None,
        images: Vec::new(),
        error: None,
        progress: "排队中".to_string(),
        duration_ms: None,
        input_tokens: 0,
        output_tokens: 0,
    };
    let view = task_view(&task, false);
    crate::image_tasks::submit(state, task, image_request, key).await;
    Ok(Json(view))
}

/// `POST /api/image-tasks/edits` —— 异步图生图：参考图与 mask 一起入队。
pub async fn submit_edit(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<TaskView>, ApiError> {
    let key = request
        .extensions()
        .get::<crate::auth::ApiKeyIdentity>()
        .cloned();
    let prepared_edit = crate::routes::v1_image_edits::prepare_edit(&state, request).await?;
    let now = chrono::Utc::now();
    let task = ImageTask {
        id: Uuid::new_v4(),
        status: TaskStatus::Queued,
        prompt: prepared_edit.prompt.clone(),
        model: prepared_edit.request.model.clone(),
        n: prepared_edit.request.n,
        response_format: prepared_edit.response_format.clone(),
        output_format: prepared_edit
            .request
            .output_format
            .map(|format| format.label().to_string()),
        frame_count: prepared_edit.request.frame_count,
        frame_duration_ms: prepared_edit.request.frame_duration_ms,
        base_url: prepared_edit.base_url.clone(),
        created_at: now,
        updated_at: now,
        finished_at: None,
        images: Vec::new(),
        error: None,
        progress: "排队中".to_string(),
        duration_ms: None,
        input_tokens: 0,
        output_tokens: 0,
    };
    let view = task_view(&task, false);
    crate::image_tasks::submit_edit(
        state,
        task,
        prepared_edit.request,
        prepared_edit.prepared,
        prepared_edit.input_image_tokens,
        key,
    )
    .await;
    Ok(Json(view))
}

/// `GET /api/image-tasks`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<TaskView>>, ApiError> {
    let ids = params.get("ids").map(|raw| parse_ids(raw)).unwrap_or_default();
    let filter = if ids.is_empty() { None } else { Some(ids.as_slice()) };
    let tasks = state.image_tasks.list(filter);
    Ok(Json(tasks.iter().map(|task| task_view(task, false)).collect()))
}

/// 任务视图里除了 `data` 之外的字段（流式响应先用它拼前缀）。
fn task_head(task: &ImageTask) -> serde_json::Map<String, serde_json::Value> {
    let view = task_view(task, false);
    let mut value = serde_json::to_value(&view).unwrap_or(serde_json::Value::Null);
    if let Some(map) = value.as_object_mut() {
        map.remove("data");
        return map.clone();
    }
    serde_json::Map::new()
}

/// `GET /api/image-tasks/{id}`
pub async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::openai(AppError::invalid_param("任务 id 不是合法 UUID", "id")))?;
    let task = state
        .image_tasks
        .get(uuid)
        .ok_or_else(|| ApiError::openai(AppError::not_found("任务不存在")))?;
    let want_b64 = task.response_format == "b64_json";
    // base64 走流：每张图按需从磁盘读，编完就丢，不整串驻留。
    let data_dir = gpt2apirs_core::paths::data_dir().ok();
    let items = task
        .images
        .iter()
        .map(|image| {
            let source = if want_b64 {
                match &data_dir {
                    Some(root) => crate::image_response::ItemSource::File(
                        root.join(image.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR)),
                    ),
                    None => crate::image_response::ItemSource::Url(format!(
                        "{}/{}",
                        task.base_url, image.rel_path
                    )),
                }
            } else {
                crate::image_response::ItemSource::Url(format!("{}/{}", task.base_url, image.rel_path))
            };
            crate::image_response::StreamItem {
                source,
                prompt: None,
            }
        })
        .collect();
    let head = task_head(&task);
    let mut prefix = serde_json::to_string(&serde_json::Value::Object(head)).unwrap_or_default();
    prefix.truncate(prefix.trim_end().len() - 1);
    prefix.push_str(",\"data\":[");
    Ok(crate::image_response::StreamedJson {
        prefix,
        items,
        suffix: "]}".to_string(),
    }
    .into_response())
}

/// `DELETE /api/image-tasks/{id}`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::openai(AppError::invalid_param("任务 id 不是合法 UUID", "id")))?;
    if !state.image_tasks.delete(uuid) {
        return Err(ApiError::openai(AppError::not_found("任务不存在")));
    }
    Ok(Json(serde_json::json!({ "deleted": true, "id": id })))
}


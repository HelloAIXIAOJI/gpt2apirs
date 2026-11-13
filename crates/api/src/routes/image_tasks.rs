//! `/api/image-tasks` —— 异步生图任务的提交与查询。
//!
//! 提交立即返回任务 id，生成在后台跑；客户端按 id 轮询状态与结果。
//! 鉴权与 `/v1/*` 一致用 API key（这是给 API 调用方用的，不是后台管理面）。

use std::sync::Arc;

use axum::Json;
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
const MAX_N: u32 = 4;

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
pub fn task_view(task: &ImageTask) -> TaskView {
    let want_b64 = task.response_format == "b64_json";
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
        created_at: task.created_at.to_rfc3339(),
        updated_at: task.updated_at.to_rfc3339(),
        finished_at: task.finished_at.map(|t| t.to_rfc3339()),
        error: task.error.clone(),
        data,
    }
}

/// `POST /api/image-tasks/generations`
pub async fn submit(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<TaskView>, ApiError> {
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
    if n == 0 || n > MAX_N {
        return Err(ApiError::openai(AppError::invalid_param(
            format!("n 必须在 1..={MAX_N}"),
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

    let base_url = state.public_base_url(&scheme, &host);
    let now = chrono::Utc::now();
    let task = ImageTask {
        id: Uuid::new_v4(),
        status: TaskStatus::Queued,
        prompt: prompt.clone(),
        model: requested_model,
        n,
        response_format,
        base_url,
        created_at: now,
        updated_at: now,
        finished_at: None,
        images: Vec::new(),
        error: None,
    };
    let view = task_view(&task);
    crate::image_tasks::submit(state, task, image_request);
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
    Ok(Json(tasks.iter().map(task_view).collect()))
}

/// `GET /api/image-tasks/{id}`
pub async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskView>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::openai(AppError::invalid_param("任务 id 不是合法 UUID", "id")))?;
    let task = state
        .image_tasks
        .get(uuid)
        .ok_or_else(|| ApiError::openai(AppError::not_found("任务不存在")))?;
    Ok(Json(task_view(&task)))
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


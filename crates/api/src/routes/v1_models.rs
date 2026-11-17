//! `GET /v1/models` 与 `GET /v1/models/{id}`。
//!
//! 目录来自 `gpt2apirs_generate::models::MODEL_CATALOG`：这里不硬编码模型名，
//! 新增模型只改那一处，否则两个端点会给出不一致的答案。
//!
//! 尚未实现按账号池动态过滤（有 Plus 账号才列 plus-* 模型）。

use axum::Json;
use axum::extract::Path;
use gpt2apirs_core::AppError;
use gpt2apirs_generate::models::{MODEL_CATALOG, ModelInfo, find};
use serde_json::{Value, json};

use crate::error::ApiError;

/// 把目录项渲染成 OpenAI 的模型对象。
fn model_json(info: &ModelInfo) -> Value {
    json!({
        "id": info.id,
        "object": "model",
        "created": 0,
        "owned_by": "gpt2apirs",
        "kind": info.kind.as_str(),
        "permission": [],
        "root": info.id,
        "parent": null,
    })
}

/// `GET /v1/models`
pub async fn list_models() -> Json<Value> {
    let data: Vec<Value> = MODEL_CATALOG.iter().map(model_json).collect();
    Json(json!({
        "object": "list",
        "data": data,
    }))
}

/// `GET /v1/models/{id}`
pub async fn get_model(Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    match find(&id) {
        Some(info) => Ok(Json(model_json(info))),
        None => Err(ApiError::openai(AppError::not_found_coded(
            format!("模型 {id:?} 不存在"),
            "model_not_found",
        ))),
    }
}

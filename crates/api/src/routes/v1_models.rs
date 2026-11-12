//! `GET /v1/models` —— OpenAI 兼容模型列表。
//!
//!
//! 尚未实现按账号池动态过滤（有 Plus 账号才列 plus-* 模型）。
//! M3 账号池落地前，这里返回**静态受支持列表**。

use axum::Json;
use serde_json::{Value, json};

/// `GET /v1/models`
pub async fn list_models() -> Json<Value> {
    let data: Vec<Value> = gpt2apirs_generate::models::SUPPORTED_MODELS
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "gpt2apirs",
                "permission": [],
                "root": id,
                "parent": null,
            })
        })
        .collect();

    Json(json!({
        "object": "list",
        "data": data,
    }))
}

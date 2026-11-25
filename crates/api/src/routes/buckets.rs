//! 账号桶的管理接口（`/api/buckets*`）。
//!
//! 桶是「一批账号 + 一个出口」。这里只做增删改查；选号逻辑在 account 池里，
//! 靠 `SelectionFilter::bucket` 生效。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use gpt2apirs_core::AppError;
use gpt2apirs_storage::repo::{BucketRepo, DEFAULT_BUCKET_ID};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

fn bad(message: impl Into<String>, param: &str) -> ApiError {
    ApiError::admin(AppError::invalid_param(message.into(), param))
}

/// 新建桶的请求。
#[derive(Debug, Deserialize)]
pub struct CreateBucketRequest {
    /// 桶名。
    pub name: String,
}

/// 修改桶的请求。
///
/// 现在只有改名：出口已经收回全局，桶不再有可配置项。
#[derive(Debug, Deserialize, Default)]
pub struct UpdateBucketRequest {
    /// 新名字。
    #[serde(default)]
    pub name: Option<String>,
}

fn view(row: &gpt2apirs_storage::repo::BucketRow) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "is_default": row.is_default,
        "account_count": row.account_count,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

/// `GET /api/buckets`
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let rows = BucketRepo::new(state.db.clone()).list().await.map_err(internal)?;
    Ok(Json(json!({
        "default_id": DEFAULT_BUCKET_ID,
        "data": rows.iter().map(view).collect::<Vec<_>>(),
    })))
}

/// `POST /api/buckets`
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateBucketRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(bad("桶名不能为空", "name"));
    }
    let repo = BucketRepo::new(state.db.clone());
    let id = repo.create(name).await.map_err(|error| {
        // 唯一索引冲突要说人话：用户分不清「数据库报错」和「重名了」
        if error.to_string().contains("buckets_name_idx") {
            bad(format!("已经有叫「{name}」的桶了"), "name")
        } else {
            internal(error)
        }
    })?;
    tracing::info!(%id, name, "新建账号桶");
    let row = repo.get(id).await.map_err(internal)?.ok_or_else(|| internal("新建后读不到桶"))?;
    Ok(Json(view(&row)))
}

/// `PATCH /api/buckets/{id}`
pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateBucketRequest>,
) -> Result<Json<Value>, ApiError> {
    let repo = BucketRepo::new(state.db.clone());
    if let Some(name) = request.name.as_deref() {
        let name = name.trim();
        if name.is_empty() {
            return Err(bad("桶名不能为空", "name"));
        }
        repo.rename(id, name).await.map_err(|error| {
            if error.to_string().contains("buckets_name_idx") {
                bad(format!("已经有叫「{name}」的桶了"), "name")
            } else {
                internal(error)
            }
        })?;
    }

    let row = repo.get(id).await.map_err(internal)?.ok_or_else(|| bad("桶不存在", "id"))?;
    Ok(Json(view(&row)))
}

/// `DELETE /api/buckets/{id}`
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    if id == DEFAULT_BUCKET_ID {
        return Err(bad(
            "默认桶不能删除：它就是「没指定桶」时的落点",
            "id",
        ));
    }
    let repo = BucketRepo::new(state.db.clone());
    match repo.delete(id).await {
        Ok(true) => Ok(Json(json!({ "deleted": 1, "id": id }))),
        Ok(false) => Err(ApiError::admin(AppError::NotFound {
            message: "桶不存在（或它是默认桶）".to_string(),
            code: Some("bucket_not_found".to_string()),
        })),
        Err(gpt2apirs_storage::pool::StorageError::BucketBusy { accounts }) => Err(bad(
            format!("这个桶里还有 {accounts} 个账号，先把它们移走或删掉再删桶"),
            "id",
        )),
        Err(error) => Err(internal(error)),
    }
}

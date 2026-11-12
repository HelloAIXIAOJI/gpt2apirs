//! 代理分组接口。
//!
//! 组地址是**真源**：改组地址会同步重写组内账号的 `proxy`；
//! 账号移出分组时保留当前生效的代理串，避免突然改路由。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use gpt2apirs_account::ProxyGroupRepo;
use gpt2apirs_core::AppError;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

/// 新建/修改请求。
#[derive(Debug, Deserialize)]
pub struct GroupRequest {
    /// 名称。
    pub name: String,
    /// 代理串，空串表示直连。
    #[serde(default)]
    pub url: String,
}

/// `GET /api/proxy-groups`
pub async fn list_groups(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let repo = ProxyGroupRepo::new(state.db.clone());
    let groups = repo.list().await.map_err(internal)?;
    Ok(Json(json!({ "data": groups })))
}

/// `POST /api/proxy-groups`
pub async fn create_group(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GroupRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.name.trim().is_empty() {
        return Err(ApiError::admin(AppError::invalid_param(
            "名称不能为空",
            "name",
        )));
    }
    let repo = ProxyGroupRepo::new(state.db.clone());
    let group = repo
        .create(&request.name, &request.url)
        .await
        .map_err(|error| {
            ApiError::admin(AppError::invalid_param(
                format!("创建分组失败（名称可能重复）：{error}"),
                "name",
            ))
        })?;
    Ok(Json(json!(group)))
}

/// `PUT /api/proxy-groups/{id}`
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(request): Json<GroupRequest>,
) -> Result<Json<Value>, ApiError> {
    let repo = ProxyGroupRepo::new(state.db.clone());
    let existing = repo.get(id).await.map_err(internal)?;
    let Some(existing) = existing else {
        return Err(ApiError::admin(AppError::invalid_param("分组不存在", "id")));
    };

    let group = repo
        .update(id, &request.name, &request.url)
        .await
        .map_err(internal)?;
    let mut synced = 0;
    if existing.url != group.url {
        synced = state
            .pool
            .repo()
            .sync_proxy_group(id, &group.url)
            .await
            .map_err(internal)?;
        let _ = state.pool.reload().await;
    }
    Ok(Json(json!({ "group": group, "synced_accounts": synced })))
}

/// `DELETE /api/proxy-groups/{id}`
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let repo = ProxyGroupRepo::new(state.db.clone());
    let detached = state
        .pool
        .repo()
        .detach_proxy_group(id)
        .await
        .map_err(internal)?;
    let removed = repo.delete(id).await.map_err(internal)?;
    if !removed {
        return Err(ApiError::admin(AppError::invalid_param("分组不存在", "id")));
    }
    let _ = state.pool.reload().await;
    Ok(Json(json!({ "ok": true, "detached_accounts": detached })))
}

/// 账号加入/移出分组的请求。
#[derive(Debug, Deserialize)]
pub struct AssignRequest {
    /// 分组 ID；`null` 表示移出。
    #[serde(default)]
    pub group_id: Option<Uuid>,
}

/// `POST /api/accounts/{id}/proxy-group`
pub async fn assign_group(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<Uuid>,
    Json(request): Json<AssignRequest>,
) -> Result<Json<Value>, ApiError> {
    let account = match request.group_id {
        Some(group_id) => {
            let repo = ProxyGroupRepo::new(state.db.clone());
            let group = repo
                .get(group_id)
                .await
                .map_err(internal)?
                .ok_or_else(|| {
                    ApiError::admin(AppError::invalid_param("分组不存在", "group_id"))
                })?;
            state
                .pool
                .repo()
                .assign_proxy_group(account_id, group_id, &group.url)
                .await
                .map_err(internal)?
        }
        None => state
            .pool
            .repo()
            .clear_proxy_group(account_id)
            .await
            .map_err(internal)?,
    };
    let _ = state.pool.reload().await;
    Ok(Json(json!({
        "id": account.id,
        "proxy": account.proxy,
        "proxy_group_id": account.proxy_group_id,
    })))
}

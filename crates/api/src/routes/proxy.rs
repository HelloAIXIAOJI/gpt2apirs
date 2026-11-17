//! 订阅代理的管理接口：状态、启停、节点列表、切换出口、测速。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use gpt2apirs_core::AppError;
use gpt2apirs_storage::repo::PgSettingsRepo;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::proxy_service::LISTEN_PORT;
use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, ApiError>;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

/// 出口代理串（本地 mixed 监听）。
fn egress_url() -> String {
    format!("http://127.0.0.1:{LISTEN_PORT}")
}

/// `GET /api/proxy/status`
pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!(state.proxy_service.status()))
}

/// 启动请求。
#[derive(Debug, Deserialize)]
pub struct StartRequest {
    /// 订阅地址；给了就先刷新订阅再启动，不给就用已缓存的节点启动。
    #[serde(default)]
    pub url: Option<String>,
    /// 默认选中的节点。
    #[serde(default)]
    pub selected: Option<String>,
}

/// `POST /api/proxy/start` —— 用已缓存节点启动；带 url 时先刷新。
pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(request): Json<StartRequest>,
) -> ApiResult<Value> {
    let selected = request
        .selected
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let url = request
        .url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let status = match url {
        Some(url) => {
            let status = state
                .proxy_service
                .refresh(url, selected)
                .await
                .map_err(internal)?;
            persist(&state, url, selected).await?;
            state
                .proxy_service
                .start_cached(selected)
                .map_err(internal)?;
            status
        }
        None => state
            .proxy_service
            .start_cached(selected)
            .map_err(internal)?,
    };
    state.egress.set(Some(&egress_url()));
    Ok(Json(json!(status)))
}

/// 刷新订阅请求。
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    /// 订阅地址；空表示用设置里已保存的地址。
    #[serde(default)]
    pub url: Option<String>,
    /// 选中节点。
    #[serde(default)]
    pub selected: Option<String>,
}

/// `POST /api/proxy/refresh` —— 只拉订阅、更新节点表，不动正在跑的出口（跑着就热重启）。
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RefreshRequest>,
) -> ApiResult<Value> {
    let url = request
        .url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| state.settings.get_string("proxy_subscription_url"));
    if url.trim().is_empty() {
        return Err(ApiError::admin(AppError::invalid_param(
            "订阅地址为空：先在设置或本请求里给出",
            "url",
        )));
    }
    let status = state
        .proxy_service
        .refresh(&url, request.selected.as_deref())
        .await
        .map_err(internal)?;
    persist(&state, &url, request.selected.as_deref()).await?;
    Ok(Json(json!(status)))
}

/// `POST /api/proxy/stop`
pub async fn stop(State(state): State<Arc<AppState>>) -> Json<Value> {
    state.proxy_service.stop();
    state.egress.set(None);
    Json(json!({ "running": false }))
}

/// `GET /api/proxy/nodes`
pub async fn nodes(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "nodes": state.proxy_service.nodes(),
        "status": state.proxy_service.status(),
    }))
}

/// 切换出口请求。
#[derive(Debug, Deserialize)]
pub struct SelectRequest {
    /// 节点名。
    pub name: String,
}

/// `POST /api/proxy/select`
pub async fn select(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SelectRequest>,
) -> ApiResult<Value> {
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::admin(AppError::invalid_param(
            "节点名不能为空",
            "name",
        )));
    }
    state.proxy_service.select(&name).await.map_err(internal)?;
    let url = state.settings.get_string("proxy_subscription_url");
    persist(&state, &url, Some(&name)).await?;
    Ok(Json(json!({ "selected": name })))
}

/// 测速请求。
#[derive(Debug, Default, Deserialize)]
pub struct ProbeRequest {
    /// 指定节点；空表示全部。
    #[serde(default)]
    pub names: Vec<String>,
    /// 探测目标。
    #[serde(default)]
    pub url: Option<String>,
    /// 单项超时。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// 并发数。
    #[serde(default)]
    pub concurrency: Option<usize>,
}

/// `POST /api/proxy/probe`
pub async fn probe(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProbeRequest>,
) -> Json<Value> {
    let probe_url = crate::proxy_service::probe_url_from(
        &request
            .url
            .unwrap_or_else(|| state.settings.get_string("proxy_probe_url")),
    );
    let timeout_ms = request
        .timeout_ms
        .unwrap_or_else(|| state.settings.get_i64("proxy_probe_timeout_ms").max(1000) as u64);
    let names = if request.names.is_empty() {
        None
    } else {
        Some(request.names)
    };
    let results = state
        .proxy_service
        .probe(
            names,
            &probe_url,
            timeout_ms,
            crate::proxy_service::probe_concurrency(state.proxy_service.status().node_count),
        )
        .await;
    Json(json!({ "results": results }))
}

async fn persist(
    state: &Arc<AppState>,
    url: &str,
    selected: Option<&str>,
) -> Result<(), ApiError> {
    let repo = PgSettingsRepo::new(state.db.clone());
    let mut values: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    values.insert("proxy_subscription_url".to_string(), json!(url));
    values.insert("proxy_enabled".to_string(), json!(1));
    if let Some(name) = selected {
        values.insert("proxy_selected_node".to_string(), json!(name));
    }
    let snapshot = repo.merge(&values).await.map_err(internal)?;
    state
        .settings
        .replace(snapshot.data.clone(), snapshot.updated_at);
    Ok(())
}

/// 供启动流程复用：若设置里已启用，按保存的地址起一次。
pub async fn start_from_settings(state: &Arc<AppState>) -> anyhow::Result<()> {
    if state.settings.get_i64("proxy_enabled") != 1 {
        return Ok(());
    }
    let url = state.settings.get_string("proxy_subscription_url");
    if url.trim().is_empty() {
        return Ok(());
    }
    let selected = state.settings.get_string("proxy_selected_node");
    let selected = if selected.trim().is_empty() {
        None
    } else {
        Some(selected.as_str())
    };
    // 启动流程：拉一次订阅拿到节点，再用这些节点起代理核。
    state.proxy_service.refresh(&url, selected).await?;
    state.proxy_service.start_cached(selected)?;
    state.egress.set(Some(&egress_url()));
    Ok(())
}


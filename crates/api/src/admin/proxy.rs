//! 代理页：看出口状态、起停订阅代理、切换节点、测速。

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::PgSettingsRepo;
use serde::Deserialize;

use super::has_session;
use super::nav;
use crate::state::AppState;

/// 一行节点。
pub struct NodeRow {
    name: String,
    kind: String,
    endpoint: String,
    current: bool,
    delay: String,
}

/// 页面。
#[derive(Template)]
#[template(path = "proxy.html")]
struct ProxyPage {
    nav: Vec<nav::NavItem>,
    flash: String,
    error: String,
    running: bool,
    proxy_url: String,
    selected: String,
    node_count: String,
    last_error: String,
    skipped: Vec<String>,
    subscription_url: String,
    nodes: Vec<NodeRow>,
}

/// 订阅表单。
#[derive(Debug, Deserialize)]
pub struct StartForm {
    /// 订阅地址。
    #[serde(default)]
    pub url: String,
}

/// 节点选择表单。
#[derive(Debug, Deserialize)]
pub struct SelectForm {
    /// 节点名。
    #[serde(default)]
    pub name: String,
}

/// 提示参数。
#[derive(Debug, Default, Deserialize)]
pub struct FlashParams {
    /// 结果提示。
    #[serde(default)]
    pub flash: Option<String>,
    /// 错误提示。
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /proxy`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<FlashParams>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(build(
        &state,
        params.flash.unwrap_or_default(),
        params.error.unwrap_or_default(),
        None,
    ))
}

/// `POST /proxy/start`
pub async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<StartForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let url = form.url.trim().to_string();
    if url.is_empty() {
        return render(build(
            &state,
            String::new(),
            "订阅地址不能为空".to_string(),
            None,
        ));
    }
    match state.proxy_service.refresh(&url, None).await.and_then(|_| {
        state.proxy_service.start_cached(None)
    }) {
        Ok(status) => {
            state
                .egress
                .set(Some(&format!("http://127.0.0.1:{}", crate::proxy_service::LISTEN_PORT)));
            let selected = status.selected.clone();
            if let Err(error) = persist(&state, &url, selected.as_deref()).await {
                return render(build(
                    &state,
                    String::new(),
                    format!("已启动，但保存配置失败：{}", error.error.message()),
                    None,
                ));
            }
            let message = format!(
                "代理已启动，{} 个节点，当前出口 {}",
                status.node_count,
                status.selected.clone().unwrap_or_else(|| "（自动）".to_string())
            );
            render(build(&state, message, String::new(), None))
        }
        Err(error) => render(build(
            &state,
            String::new(),
            format!("启动失败：{error}"),
            None,
        )),
    }
}

/// `POST /proxy/start-cached` —— 用已缓存节点启动（不拉订阅）。
pub async fn start_cached(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match state.proxy_service.start_cached(None) {
        Ok(status) => {
            state
                .egress
                .set(Some(&format!("http://127.0.0.1:{}", crate::proxy_service::LISTEN_PORT)));
            let message = format!(
                "已用缓存节点启动，{} 个节点，当前出口 {}",
                status.node_count,
                status.selected.clone().unwrap_or_else(|| "（自动）".to_string())
            );
            render(build(&state, message, String::new(), None))
        }
        Err(error) => render(build(
            &state,
            String::new(),
            format!("启动失败：{error}"),
            None,
        )),
    }
}

/// `POST /proxy/refresh` —— 只拉订阅更新节点，不动出口（跑着就热重启）。
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<StartForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let mut url = form.url.trim().to_string();
    if url.is_empty() {
        url = state.settings.get_string("proxy_subscription_url");
    }
    if url.trim().is_empty() {
        return render(build(
            &state,
            String::new(),
            "订阅地址不能为空".to_string(),
            None,
        ));
    }
    match state.proxy_service.refresh(&url, None).await {
        Ok(status) => {
            if let Err(error) = persist(&state, &url, status.selected.as_deref()).await {
                return render(build(
                    &state,
                    String::new(),
                    format!("订阅已更新，但保存配置失败：{}", error.error.message()),
                    None,
                ));
            }
            let message = format!("订阅已刷新，当前 {} 个节点", status.node_count);
            render(build(&state, message, String::new(), None))
        }
        Err(error) => render(build(
            &state,
            String::new(),
            format!("刷新订阅失败：{error}"),
            None,
        )),
    }
}

/// `POST /proxy/stop`
pub async fn stop(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    state.proxy_service.stop();
    state.egress.set(None);
    render(build(
        &state,
        "代理已停止，出口回到直连".to_string(),
        String::new(),
        None,
    ))
}

/// `POST /proxy/select`
pub async fn select(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<SelectForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return render(build(
            &state,
            String::new(),
            "节点名不能为空".to_string(),
            None,
        ));
    }
    match state.proxy_service.select(&name).await {
        Ok(()) => {
            let url = state.settings.get_string("proxy_subscription_url");
            if let Err(error) = persist(&state, &url, Some(&name)).await {
                tracing::warn!(error = %error.error.message(), "保存出口节点失败");
            }
            render(build(
                &state,
                format!("出口已切到 {name}"),
                String::new(),
                None,
            ))
        }
        Err(error) => render(build(
            &state,
            String::new(),
            format!("切换失败：{error}"),
            None,
        )),
    }
}

/// 单批测速请求：前端按 10 并发逐节点调用，自己更新单元格。
#[derive(Debug, Default, Deserialize)]
pub struct ProbeBatch {
    /// 要测的节点名；空表示全部。
    #[serde(default)]
    pub names: Vec<String>,
}

/// `POST /proxy/probe.json` —— 返回测速结果，供页面用 JS 就地更新。
pub async fn probe_json(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(batch): axum::Json<ProbeBatch>,
) -> Response {
    if !has_session(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let url = crate::proxy_service::probe_url_from(&state.settings.get_string("proxy_probe_url"));
    let timeout = state.settings.get_i64("proxy_probe_timeout_ms").max(1000) as u64;
    let names = if batch.names.is_empty() {
        None
    } else {
        Some(batch.names)
    };
    // 页面侧按 10 并发自行调度，这里每批也只开 10。
    let results = state.proxy_service.probe(names, &url, timeout, 10).await;
    axum::Json(serde_json::json!({ "results": results })).into_response()
}

/// `POST /proxy/clearance` —— 用当前出口测一次能不能过 Cloudflare。
pub async fn clearance(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let outcome = crate::clearance::probe(&state, crate::clearance::DEFAULT_CLEARANCE_URL, 15_000).await;
    let message = format!("Cloudflare 探测：{}", outcome.as_text());
    let error = if outcome.is_ok() {
        String::new()
    } else {
        message.clone()
    };
    let flash = if outcome.is_ok() { message } else { String::new() };
    render(build(&state, flash, error, None))
}

/// `POST /proxy/probe` —— 全部节点测速后直接渲染结果。
pub async fn probe(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let url = crate::proxy_service::probe_url_from(&state.settings.get_string("proxy_probe_url"));
    let timeout = state.settings.get_i64("proxy_probe_timeout_ms").max(1000) as u64;
    let concurrency = crate::proxy_service::probe_concurrency(state.proxy_service.status().node_count);
    let results = state
        .proxy_service
        .probe(None, &url, timeout, concurrency)
        .await;
    let mut delays: HashMap<String, String> = HashMap::new();
    for item in results {
        let text = match (item.delay_ms, item.error) {
            (Some(delay), _) => format!("{delay} ms"),
            (None, Some(reason)) => format!("失败：{reason}"),
            (None, None) => "-".to_string(),
        };
        delays.insert(item.name, text);
    }
    render(build(
        &state,
        "测速完成".to_string(),
        String::new(),
        Some(delays),
    ))
}

fn build(
    state: &AppState,
    flash: String,
    error: String,
    delays: Option<HashMap<String, String>>,
) -> ProxyPage {
    let status = state.proxy_service.status();
    let nodes = state
        .proxy_service
        .nodes()
        .into_iter()
        .map(|node| {
            let delay = delays
                .as_ref()
                .and_then(|map| map.get(&node.name).cloned())
                .unwrap_or_else(|| "-".to_string());
            NodeRow {
                current: status.selected.as_deref() == Some(node.name.as_str()),
                name: node.name,
                kind: node.kind,
                endpoint: format!("{}:{}", node.server, node.port),
                delay,
            }
        })
        .collect();
    ProxyPage {
        nav: nav::items("/proxy"),
        flash,
        error,
        running: status.running,
        proxy_url: status.proxy_url.unwrap_or_default(),
        selected: status.selected.unwrap_or_default(),
        node_count: status.node_count.to_string(),
        last_error: status.last_error.unwrap_or_default(),
        skipped: status.skipped,
        subscription_url: state.settings.get_string("proxy_subscription_url"),
        nodes,
    }
}

async fn persist(
    state: &Arc<AppState>,
    url: &str,
    selected: Option<&str>,
) -> Result<(), crate::error::ApiError> {
    let repo = PgSettingsRepo::new(state.db.clone());
    let mut values: HashMap<String, serde_json::Value> = HashMap::new();
    values.insert("proxy_subscription_url".to_string(), serde_json::json!(url));
    values.insert("proxy_enabled".to_string(), serde_json::json!(1));
    if let Some(name) = selected {
        values.insert("proxy_selected_node".to_string(), serde_json::json!(name));
    }
    let snapshot = repo
        .merge(&values)
        .await
        .map_err(|error| {
            crate::error::ApiError::admin(gpt2apirs_core::AppError::Internal(anyhow::anyhow!(
                "{error}"
            )))
        })?;
    state
        .settings
        .replace(snapshot.data.clone(), snapshot.updated_at);
    Ok(())
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("模板渲染失败：{error}"),
        )
            .into_response(),
    }
}


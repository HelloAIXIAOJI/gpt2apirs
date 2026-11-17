//! 任务页：看异步生图任务的进度、结果与失败原因。
//!
//! 任务存在内存里（进程重启即丢），所以这里不做持久化展示，只读当前快照。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::accounts::encode;
use super::has_session;
use super::nav;
use crate::state::AppState;

/// 结果缩略图。
pub struct ResultThumb {
    url: String,
}

/// 一行任务。
pub struct TaskRow {
    id: String,
    status: String,
    status_class: String,
    prompt: String,
    model: String,
    n: String,
    created_at: String,
    duration: String,
    error: String,
    images: Vec<ResultThumb>,
}

/// 页面。
#[derive(Template)]
#[template(path = "tasks.html")]
struct TasksPage {
    nav: Vec<nav::NavItem>,
    flash: String,
    error: String,
    rows: Vec<TaskRow>,
    running: usize,
    poll: bool,
}

/// 提示参数。
#[derive(Debug, Default, Deserialize)]
pub struct FlashParams {
    /// 操作结果提示。
    #[serde(default)]
    pub flash: Option<String>,
}

/// 任务操作目标。
#[derive(Debug, Deserialize)]
pub struct TaskForm {
    /// 任务 id。
    #[serde(default)]
    pub id: String,
}

/// `GET /tasks`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<FlashParams>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(page(&state, params.flash.unwrap_or_default()))
}

/// `GET /tasks/panel` —— 只回面板，供自动刷新。
pub async fn panel(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(panel_only(&state))
}

#[derive(Template)]
#[template(path = "tasks_panel.html")]
struct TasksPanel {
    rows: Vec<TaskRow>,
    running: usize,
    poll: bool,
    error: String,
}

fn panel_only(state: &AppState) -> TasksPanel {
    let (rows, running) = load(state);
    TasksPanel {
        rows,
        running,
        poll: running > 0,
        error: String::new(),
    }
}

fn page(state: &AppState, flash: String) -> TasksPage {
    let (rows, running) = load(state);
    TasksPage {
        nav: nav::items("/tasks"),
        flash,
        error: String::new(),
        rows,
        running,
        poll: running > 0,
    }
}

fn load(state: &AppState) -> (Vec<TaskRow>, usize) {
    let tasks = state.image_tasks.list(None);
    let running = tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    let rows = tasks
        .iter()
        .map(|task| {
            let duration = match task.finished_at {
                Some(finished) => format!(
                    "{:.1}s",
                    (finished - task.created_at).num_milliseconds() as f64 / 1000.0
                ),
                None => "-".to_string(),
            };
            TaskRow {
                id: task.id.to_string(),
                status: task.status.as_str().to_string(),
                status_class: match task.status.as_str() {
                    "succeeded" => "badge badge--ok".to_string(),
                    "failed" => "badge badge--danger".to_string(),
                    "running" => "badge badge--warn".to_string(),
                    _ => "badge".to_string(),
                },
                prompt: task.prompt.chars().take(60).collect(),
                model: task.model.clone(),
                n: task.n.to_string(),
                created_at: task.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
                duration,
                error: task.error.clone().unwrap_or_default(),
                images: task
                    .images
                    .iter()
                    .map(|image| ResultThumb {
                        url: format!("{}/{}", task.base_url, image.rel_path),
                    })
                    .collect(),
            }
        })
        .collect();
    (rows, running)
}

/// `POST /tasks/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<TaskForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let message = match Uuid::parse_str(form.id.trim()) {
        Ok(id) => {
            if state.image_tasks.delete(id) {
                format!("已删除任务 {}", &id.to_string()[..8])
            } else {
                "任务不存在（可能已过期淘汰）".to_string()
            }
        }
        Err(_) => "任务 id 不合法".to_string(),
    };
    Redirect::to(&format!("/tasks?flash={}", encode(&message))).into_response()
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

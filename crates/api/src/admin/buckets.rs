//! 账号桶页：新建、改名、设出口、删除。
//!
//! 桶里还有账号时不能删（DB 用 `on delete restrict` 兜底，这里先给出人话）。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::{BucketRepo, BucketRow, DEFAULT_BUCKET_ID};
use serde::Deserialize;
use uuid::Uuid;

use super::{has_session, nav};
use crate::state::AppState;

/// 表格里的一行。
pub struct Row {
    id: String,
    name: String,
    url: String,
    accounts: i64,
    is_default: bool,
}

impl Row {
    fn from(row: &BucketRow) -> Self {
        Self {
            id: row.id.to_string(),
            name: row.name.clone(),
            url: row.proxy_url.clone(),
            accounts: row.account_count,
            is_default: row.is_default,
        }
    }

}

/// 桶页。
#[derive(Template)]
#[template(path = "buckets.html")]
struct BucketsPage {
    nav: Vec<nav::NavItem>,
    rows: Vec<Row>,
    error: String,
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

async fn page(state: &AppState, error: String) -> BucketsPage {
    let (rows, list_error) = match BucketRepo::new(state.db.clone()).list().await {
        Ok(rows) => (rows.iter().map(Row::from).collect(), error),
        Err(failure) => (Vec::new(), format!("读取桶失败（{failure}）")),
    };
    BucketsPage {
        nav: nav::items("/buckets"),
        rows,
        error: list_error,
    }
}

/// `GET /buckets`
pub async fn index(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(page(&state, String::new()).await)
}

/// 新建表单。
#[derive(Debug, Deserialize)]
pub struct CreateForm {
    /// 桶名。
    #[serde(default)]
    pub name: String,
}

/// `POST /buckets/create`
pub async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.name.trim();
    if name.is_empty() {
        return render(page(&state, "桶名不能为空".to_string()).await);
    }
    let repo = BucketRepo::new(state.db.clone());
    match repo.create(name).await {
        Ok(_) => render(page(&state, String::new()).await),
        Err(failure) if failure.to_string().contains("buckets_name_idx") => {
            render(page(&state, format!("已经有叫「{name}」的桶了")).await)
        }
        Err(failure) => render(page(&state, format!("新建失败：{failure}")).await),
    }
}

/// 改名表单。
#[derive(Debug, Deserialize)]
pub struct RenameForm {
    /// 新名字。
    #[serde(default)]
    pub name: String,
}

/// `POST /buckets/{id}/rename`
pub async fn rename(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<RenameForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.name.trim();
    if name.is_empty() {
        return render(page(&state, "桶名不能为空".to_string()).await);
    }
    match BucketRepo::new(state.db.clone()).rename(id, name).await {
        Ok(_) => render(page(&state, String::new()).await),
        Err(failure) if failure.to_string().contains("buckets_name_idx") => {
            render(page(&state, format!("已经有叫「{name}」的桶了")).await)
        }
        Err(failure) => render(page(&state, format!("改名失败：{failure}")).await),
    }
}

/// 出口表单：节点名与自定义地址二选一。
#[derive(Debug, Deserialize)]
pub struct EgressForm {
    /// 出口地址；空 = 跟随全局出口。
    ///
    /// 订阅节点是按「全局一个代理核」设计的，做不到每桶各选一个节点
    /// （那需要每桶起一个核进程），所以这里只收地址。
    #[serde(default)]
    pub proxy_url: String,
}

/// `POST /buckets/{id}/egress`
pub async fn set_egress(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<EgressForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match BucketRepo::new(state.db.clone())
        .set_egress(id, "", &form.proxy_url)
        .await
    {
        Ok(_) => render(page(&state, String::new()).await),
        Err(failure) => render(page(&state, format!("保存出口失败：{failure}")).await),
    }
}

/// `POST /buckets/{id}/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    if id == DEFAULT_BUCKET_ID {
        return render(page(&state, "默认桶不能删除".to_string()).await);
    }
    match BucketRepo::new(state.db.clone()).delete(id).await {
        Ok(_) => render(page(&state, String::new()).await),
        Err(gpt2apirs_storage::pool::StorageError::BucketBusy { accounts }) => render(
            page(
                &state,
                format!("这个桶里还有 {accounts} 个账号：先移到别的桶或删掉，才能删桶"),
            )
            .await,
        ),
        Err(failure) => render(page(&state, format!("删除失败：{failure}")).await),
    }
}

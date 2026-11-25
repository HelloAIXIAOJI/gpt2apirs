//! 桶的新建页与编辑页。
//!
//! 列表页只管看和进，建立/改名/删除都在这里 —— 与账号、密钥的编辑页一致。
//! 出口已经收回全局（代理页），桶不再有可配置项，所以编辑页只有名字和删除。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::{BucketRepo, DEFAULT_BUCKET_ID};
use serde::Deserialize;
use uuid::Uuid;

use super::{has_session, nav};
use crate::state::AppState;

/// 路由里的桶标识：`default` 是默认桶别名，模板不散落固定 UUID。
fn bucket_id(raw: &str) -> Option<Uuid> {
    match raw.trim() {
        "default" => Some(DEFAULT_BUCKET_ID),
        other => other.parse::<Uuid>().ok(),
    }
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

fn back_to_accounts(flash: &str) -> Response {
    Redirect::to(&format!("/accounts?flash={}", super::accounts::encode(flash))).into_response()
}

/// 新建页。
#[derive(Template)]
#[template(path = "buckets_new.html")]
struct NewPage {
    nav: Vec<nav::NavItem>,
    flash: String,
}

/// `GET /buckets/new`
pub async fn new_form(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(NewPage {
        nav: nav::items("/accounts"),
        flash: String::new(),
    })
}

/// 新建/改名共用的表单。
#[derive(Debug, Deserialize)]
pub struct NameForm {
    /// 桶名。
    #[serde(default)]
    pub name: String,
}

/// `POST /buckets/new`
pub async fn new_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NameForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.name.trim();
    if name.is_empty() {
        return render(NewPage {
            nav: nav::items("/accounts"),
            flash: "桶名不能为空".to_string(),
        });
    }
    match BucketRepo::new(state.db.clone()).create(name).await {
        Ok(_) => back_to_accounts(&format!("已新建桶「{name}」")),
        Err(failure) if failure.to_string().contains("buckets_name_idx") => render(NewPage {
            nav: nav::items("/accounts"),
            flash: format!("已经有叫「{name}」的桶了"),
        }),
        Err(failure) => render(NewPage {
            nav: nav::items("/accounts"),
            flash: format!("新建失败：{failure}"),
        }),
    }
}

/// 编辑页。
#[derive(Template)]
#[template(path = "buckets_edit.html")]
struct EditPage {
    nav: Vec<nav::NavItem>,
    /// 路由用的标识（`default` 或 UUID）。
    slug: String,
    name: String,
    is_default: bool,
    accounts: i64,
    /// 能否删除：非默认桶且空。
    can_delete: bool,
    flash: String,
}

async fn load_edit(state: &AppState, slug: &str, flash: String) -> Result<EditPage, Response> {
    let Some(id) = bucket_id(slug) else {
        return Err(back_to_accounts("桶标识无效"));
    };
    let row = match BucketRepo::new(state.db.clone()).get(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(back_to_accounts("桶不存在")),
        Err(failure) => return Err(back_to_accounts(&format!("读取失败：{failure}"))),
    };
    Ok(EditPage {
        nav: nav::items("/accounts"),
        slug: slug.to_string(),
        name: row.name,
        is_default: row.is_default,
        accounts: row.account_count,
        can_delete: !row.is_default && row.account_count == 0,
        flash,
    })
}

/// `GET /buckets/{id}/edit`
pub async fn edit_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    axum::extract::Query(query): axum::extract::Query<super::accounts::FlashQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match load_edit(&state, &slug, query.flash).await {
        Ok(page) => render(page),
        Err(response) => response,
    }
}

/// `POST /buckets/{id}/edit`
pub async fn edit_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Form(form): Form<NameForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let Some(id) = bucket_id(&slug) else {
        return back_to_accounts("桶标识无效");
    };
    let name = form.name.trim();
    if name.is_empty() {
        return Redirect::to(&format!("/buckets/{slug}/edit?flash={}", super::accounts::encode("桶名不能为空")))
            .into_response();
    }
    match BucketRepo::new(state.db.clone()).rename(id, name).await {
        Ok(_) => back_to_accounts(&format!("已改名为「{name}」")),
        Err(failure) if failure.to_string().contains("buckets_name_idx") => {
            Redirect::to(&format!(
                "/buckets/{slug}/edit?flash={}",
                super::accounts::encode(&format!("已经有叫「{name}」的桶了"))
            ))
            .into_response()
        }
        Err(failure) => Redirect::to(&format!(
            "/buckets/{slug}/edit?flash={}",
            super::accounts::encode(&format!("改名失败：{failure}"))
        ))
        .into_response(),
    }
}

/// `POST /buckets/{id}/delete` —— 只能从编辑页提交。
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let Some(id) = bucket_id(&slug) else {
        return back_to_accounts("桶标识无效");
    };
    if id == DEFAULT_BUCKET_ID {
        return back_to_accounts("默认桶不能删除");
    }
    match BucketRepo::new(state.db.clone()).delete(id).await {
        Ok(true) => back_to_accounts("桶已删除"),
        Ok(false) => back_to_accounts("桶不存在"),
        Err(gpt2apirs_storage::pool::StorageError::BucketBusy { accounts }) => back_to_accounts(
            &format!("这个桶里还有 {accounts} 个账号：先移走或删掉，才能删桶"),
        ),
        Err(failure) => back_to_accounts(&format!("删除失败：{failure}")),
    }
}

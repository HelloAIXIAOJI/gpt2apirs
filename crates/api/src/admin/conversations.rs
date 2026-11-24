//! 会话页：列出某个账号在官网留下的会话，可逐条或整页删除。
//!
//! 为什么按账号查：官网会话是**账号私有**的，多号池没有「全局会话列表」。
//! 默认取第一个可用号，页面上可以切换。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::{has_session, nav};
use crate::state::AppState;

/// 每页条数（与网页端一致）。
const PAGE_SIZE: i64 = 28;

/// 页面参数。
#[derive(Debug, Deserialize, Default)]
pub struct Filters {
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 页码，从 1 开始。
    #[serde(default)]
    pub page: i64,
    /// 上一步删了几条（回显用）。
    #[serde(default)]
    pub deleted: i64,
}

impl Filters {
    fn page(&self) -> i64 {
        self.page.max(1)
    }

    fn offset(&self) -> i64 {
        (self.page() - 1) * PAGE_SIZE
    }

    fn link(&self, account: Option<Uuid>, page: i64) -> String {
        match account {
            Some(id) => format!("/conversations?account={id}&page={page}"),
            None => format!("/conversations?page={page}"),
        }
    }

}

/// 表格里的一行。
pub struct Row {
    /// 会话 ID（删除表单要用）。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 创建时间。
    pub created: String,
    /// 更新时间。
    pub updated: String,
    /// 标记（归档 / 临时 / 不记忆）。
    pub flags: String,
}

/// 账号下拉项。
pub struct AccountOption {
    /// 账号 ID。
    pub id: String,
    /// 显示名。
    pub label: String,
    /// 是否选中。
    pub selected: bool,
}

/// 会话页。
#[derive(Template)]
#[template(path = "conversations.html")]
struct ConversationsPage {
    nav: Vec<nav::NavItem>,
    rows: Vec<Row>,
    accounts: Vec<AccountOption>,
    account: String,
    account_label: String,
    page: i64,
    total: i64,
    error: String,
    deleted: i64,
    prev_link: String,
    next_link: String,
    has_prev: bool,
    has_next: bool,
}

/// `GET /conversations`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let accounts = crate::library::selectable_accounts(&state).await;
    // 没指定账号就用第一个：页面上永远有一个「正在看的号」
    let account = filters.account.or_else(|| accounts.first().map(|(id, _)| *id));
    let account_label = account
        .and_then(|id| accounts.iter().find(|(other, _)| *other == id).map(|(_, label)| label.clone()))
        .unwrap_or_default();

    let mut error = String::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut total = 0i64;
    match account {
        Some(_) => match crate::library::conversations(&state, account, filters.offset(), PAGE_SIZE).await {
            Ok((page, _)) => {
                total = page.total;
                rows = page
                    .items
                    .iter()
                    .map(|item| {
                        let mut flags: Vec<&str> = Vec::new();
                        if item.archived {
                            flags.push("归档");
                        }
                        if item.temporary {
                            flags.push("临时");
                        }
                        if item.no_memory {
                            flags.push("不记忆");
                        }
                        Row {
                            id: item.id.clone(),
                            title: gpt2apirs_protocol::conversations::ConversationPage::display_title(item),
                            created: item
                                .created_at
                                .map(|value| value.format("%m-%d %H:%M").to_string())
                                .unwrap_or_default(),
                            updated: item
                                .updated_at
                                .map(|value| value.format("%m-%d %H:%M").to_string())
                                .unwrap_or_default(),
                            flags: flags.join(" · "),
                        }
                    })
                    .collect();
            }
            Err(failure) => error = format!("读取会话失败：{}", failure.error),
        },
        None => error = "没有可用账号：先去「账号」页导入并确认状态正常".to_string(),
    }

    let page = filters.page();
    let page_count = if total > 0 { (total + PAGE_SIZE - 1) / PAGE_SIZE } else { 1 };
    let options = accounts
        .iter()
        .map(|(id, label)| AccountOption {
            id: id.to_string(),
            label: label.clone(),
            selected: Some(*id) == account,
        })
        .collect();

    render(ConversationsPage {
        nav: nav::items("/conversations"),
        rows,
        accounts: options,
        account: account.map(|id| id.to_string()).unwrap_or_default(),
        account_label,
        page,
        total,
        error,
        deleted: filters.deleted,
        prev_link: filters.link(account, (page - 1).max(1)),
        next_link: filters.link(account, (page + 1).min(page_count.max(1))),
        has_prev: page > 1,
        has_next: page < page_count,
    })
}

/// 删除表单。
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 页码（删完回到这一页）。
    #[serde(default)]
    pub page: i64,
    /// 要删的会话（单条时一个，整页时多个）。
    #[serde(default)]
    pub id: Vec<String>,
}

/// `POST /conversations/delete` —— 单条与整页共用：表单里带几个 id 就删几个。
pub async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let mut deleted = 0i64;
    for id in form.id.iter().filter(|id| !id.trim().is_empty()) {
        match crate::library::delete_conversation(&state, form.account, id.trim()).await {
            Ok(_) => deleted += 1,
            Err(error) => {
                tracing::warn!(%error.error, conversation = %id, "删除会话失败");
            }
        }
    }
    let page = form.page.max(1);
    let target = match form.account {
        Some(id) => format!("/conversations?account={id}&page={page}&deleted={deleted}"),
        None => format!("/conversations?page={page}&deleted={deleted}"),
    };
    Redirect::to(&target).into_response()
}

/// `POST /conversations/{id}/delete` —— 行内删除（地址里带 id，表单只带账号与页码）。
pub async fn remove_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeleteForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let deleted = match crate::library::delete_conversation(&state, form.account, &id).await {
        Ok(_) => 1,
        Err(error) => {
            tracing::warn!(%error.error, %id, "删除会话失败");
            0
        }
    };
    let page = form.page.max(1);
    let target = match form.account {
        Some(account) => format!("/conversations?account={account}&page={page}&deleted={deleted}"),
        None => format!("/conversations?page={page}&deleted={deleted}"),
    };
    Redirect::to(&target).into_response()
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

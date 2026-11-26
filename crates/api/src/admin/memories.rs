//! 记忆页：查看并删除该账号在官网保存的记忆条目。
//!
//! 记忆是**账号私有**的（/backend-api/memories 需要该账号的 token），
//! 所以和会话页一样按账号查，默认取第一个可用号。

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

/// 页面参数。
#[derive(Debug, Deserialize, Default)]
pub struct Filters {
    /// 上一次操作的提示（开关 / 清空的结果）。
    #[serde(default)]
    pub flash: String,
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 上一步删了几条。
    #[serde(default)]
    pub deleted: i64,
}

/// 列表里的一条记忆。
pub struct Entry {
    /// 记忆 ID；为空表示上游没给 id，只展示不可删。
    pub id: String,
    /// 正文。
    pub content: String,
    /// 能不能删。
    pub deletable: bool,
}

/// 账号下拉项（与会话页同构）。
pub struct AccountOption {
    /// 账号 ID。
    pub id: String,
    /// 显示名。
    pub label: String,
    /// 是否选中。
    pub selected: bool,
}

/// 记忆页。
#[derive(Template)]
#[template(path = "memories.html")]
struct MemoriesPage {
    nav: Vec<nav::NavItem>,
    entries: Vec<Entry>,
    accounts: Vec<AccountOption>,
    account: String,
    account_label: String,
    error: String,
    deleted: i64,
    used: i64,
    capacity: i64,
    used_percent: u32,
    /// 记忆总开关：`on` / `off` / `unknown`（读不到，不等于关着）。
    enabled: String,
    /// 开关 / 清空的结果提示。
    flash: String,
    /// 开关按钮的文案（下一步动作）。
    toggle_label: String,
}

/// `GET /memories`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let accounts = crate::library::selectable_accounts(&state).await;
    let account = filters.account.or_else(|| accounts.first().map(|(id, _)| *id));
    let account_label = account
        .and_then(|id| accounts.iter().find(|(other, _)| *other == id).map(|(_, label)| label.clone()))
        .unwrap_or_default();

    let mut error = String::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut used = 0i64;
    let mut capacity = 0i64;
    let mut used_percent = 0u32;
    match account {
        Some(_) => match crate::library::memories(&state, account).await {
            Ok((page, _)) => {
                used = page.num_tokens;
                capacity = page.max_tokens;
                used_percent = page.used_percent().unwrap_or(0);
                entries = page
                    .entries
                    .iter()
                    .map(|entry| Entry {
                        id: entry.id.clone(),
                        content: entry.content.clone(),
                        deletable: !entry.id.is_empty(),
                    })
                    .collect();
            }
            Err(failure) => error = format!("读取记忆失败：{}", failure.error),
        },
        None => error = "没有可用账号：先去「账号」页导入并确认状态正常".to_string(),
    }

    let options = accounts
        .iter()
        .map(|(id, label)| AccountOption {
            id: id.to_string(),
            label: label.clone(),
            selected: Some(*id) == account,
        })
        .collect();

    // 开关状态要多打一次上游（settings/user）；失败不影响记忆列表本身
    let (enabled, toggle_label) = match account {
        Some(_) => match crate::library::memory_enabled(&state, account).await {
            Ok((Some(true), _)) => ("on".to_string(), "关闭记忆".to_string()),
            Ok((Some(false), _)) => ("off".to_string(), "开启记忆".to_string()),
            Ok((None, _)) => ("unknown".to_string(), "开启记忆".to_string()),
            Err(_) => ("unknown".to_string(), "开启记忆".to_string()),
        },
        None => ("unknown".to_string(), "开启记忆".to_string()),
    };

    render(MemoriesPage {
        nav: nav::items("/memories"),
        entries,
        accounts: options,
        account: account.map(|id| id.to_string()).unwrap_or_default(),
        account_label,
        error,
        deleted: filters.deleted,
        used,
        capacity,
        used_percent,
        enabled,
        toggle_label,
        flash: filters.flash.clone(),
    })
}

/// 删除表单。
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 要删的记忆（单条一个，越多条越省事）。
    #[serde(default)]
    pub id: Vec<String>,
}

/// 开关记忆的表单。
#[derive(Debug, Deserialize)]
pub struct MemoryToggleForm {
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
    /// 目标状态：`on` / `off`。
    #[serde(default)]
    pub enabled: String,
}

/// `POST /memories/setting` —— 开 / 关记忆总开关。
pub async fn toggle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MemoryToggleForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let want = form.enabled.trim() == "on";
    let flash = match crate::library::set_memory_enabled(&state, form.account, want).await {
        Ok(_) => format!("记忆已{}", if want { "开启" } else { "关闭" }),
        Err(failure) => format!("设置失败：{}", failure.error),
    };
    redirect_back(form.account, &flash)
}

/// 清空已保存记忆的表单。
#[derive(Debug, Deserialize)]
pub struct ClearForm {
    /// 账号。
    #[serde(default)]
    pub account: Option<Uuid>,
}

/// `POST /memories/clear` —— 清空这个账号已保存的记忆（不动开关）。
pub async fn clear(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ClearForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let flash = match crate::library::clear_memories(&state, form.account).await {
        Ok(_) => "已清空保存的记忆".to_string(),
        Err(failure) => format!("清空失败：{}", failure.error),
    };
    redirect_back(form.account, &flash)
}

/// 回到记忆页并带上提示。
fn redirect_back(account: Option<Uuid>, flash: &str) -> Response {
    let mut encoded = String::new();
    for ch in flash.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => encoded.push(ch),
            other => {
                for byte in other.to_string().bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    let account = account.map(|id| format!("&account={id}")).unwrap_or_default();
    Redirect::to(&format!("/memories?flash={encoded}{account}")).into_response()
}

/// `POST /memories/delete` —— 单条与多选共用。
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
        match crate::library::delete_memory(&state, form.account, id.trim()).await {
            Ok(_) => deleted += 1,
            Err(error) => tracing::warn!(%error.error, memory = %id, "删除记忆失败"),
        }
    }
    let target = match form.account {
        Some(account) => format!("/memories?account={account}&deleted={deleted}"),
        None => format!("/memories?deleted={deleted}"),
    };
    Redirect::to(&target).into_response()
}

/// `POST /memories/{id}/delete`
pub async fn remove_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeleteForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let deleted = match crate::library::delete_memory(&state, form.account, &id).await {
        Ok(_) => 1,
        Err(error) => {
            tracing::warn!(%error.error, %id, "删除记忆失败");
            0
        }
    };
    let target = match form.account {
        Some(account) => format!("/memories?account={account}&deleted={deleted}"),
        None => format!("/memories?deleted={deleted}"),
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

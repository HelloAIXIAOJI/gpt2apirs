//! 账号池管理页：列表、筛选、分页与行内动作。
//!
//! 行内动作直接复用 `/api/accounts` 的处理函数，避免同一份逻辑写两遍；
//! 每个动作只回一行 `<tr>` 或一个 toast，页面其余部分不重绘。

use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_account::{Account, AccountFilter, AccountRepo, AccountStatus, Plan};
use serde::Deserialize;
use uuid::Uuid;

use super::has_session;
use super::nav;
use crate::routes::accounts::{BatchRequest, UpdateAccountRequest};
use crate::state::AppState;

/// 每页条数。
const PAGE_SIZE: i64 = 20;

/// 列表筛选参数。
#[derive(Debug, Default, Deserialize)]
pub struct Filters {
    /// 状态。
    #[serde(default)]
    pub status: String,
    /// 套餐。
    #[serde(default)]
    pub plan: String,
    /// 关键词。
    #[serde(default)]
    pub keyword: String,
    /// 偏移。
    #[serde(default)]
    pub offset: i64,
}

impl Filters {
    fn query(&self, offset: i64) -> String {
        format!(
            "?status={}&plan={}&keyword={}&offset={}",
            encode(&self.status),
            encode(&self.plan),
            encode(&self.keyword),
            offset
        )
    }

    fn to_filter(&self) -> AccountFilter {
        AccountFilter {
            status: AccountStatus::parse(self.status.trim()),
            plan: if self.plan.trim().is_empty() {
                None
            } else {
                Some(Plan::parse(self.plan.trim()))
            },
            source: None,
            keyword: Some(self.keyword.trim().to_string()).filter(|value| !value.is_empty()),
            offset: self.offset.max(0),
            limit: PAGE_SIZE,
        }
    }
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 表格里的一行。
pub struct AccountRow {
    id: Uuid,
    email: String,
    status: String,
    status_label: String,
    plan: String,
    quota: String,
    counters: String,
    proxy: String,
    last_error: String,
    toggle_label: String,
    toggle_status: String,
}

impl AccountRow {
    fn from_account(account: &Account) -> Self {
    let disabled = account.status == AccountStatus::Disabled;
        Self {
            id: account.id,
            email: account
                .email
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| account.id.to_string()),
            status: account.status.as_str().to_string(),
            status_label: account.status.label().to_string(),
            plan: account.plan.as_str().to_string(),
            quota: account.quota.to_string(),
            counters: format!("{} / {}", account.success_count, account.fail_count),
            proxy: if account.proxy.trim().is_empty() {
                "全局出口".to_string()
            } else {
                account.proxy.clone()
            },
            last_error: account.last_error.clone().unwrap_or_default(),
            toggle_label: if disabled { "启用" } else { "禁用" }.to_string(),
            toggle_status: if disabled { "normal" } else { "disabled" }.to_string(),
        }
    }
}

impl fmt::Display for AccountRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<tr id=\"account-{}\"><td>{}</td>",
            self.id,
            escape(&self.email)
        )?;
        write!(
            f,
            "<td><span class=\"badge badge--{}\">{}</span></td>",
            escape(&self.status),
            escape(&self.status_label)
        )?;
        write!(f, "<td>{}</td>", escape(&self.plan))?;
        write!(f, "<td class=\"num\">{}</td>", escape(&self.quota))?;
        write!(f, "<td class=\"num\">{}</td>", escape(&self.counters))?;
        write!(f, "<td class=\"muted\">{}</td>", escape(&self.proxy))?;
        write!(
            f,
            "<td class=\"muted\" title=\"{}\">{}</td>",
            escape(&self.last_error),
            truncate(&self.last_error, 24)
        )?;
        write!(f, "<td class=\"actions\">")?;
        write!(
            f,
            "<button class=\"btn btn--ghost\" hx-post=\"/accounts/{}/status\" hx-vals='{{\"status\":\"{}\"}}' hx-target=\"closest tr\" hx-swap=\"outerHTML\" hx-disabled-elt=\"this\">{}</button>",
            self.id,
            escape(&self.toggle_status),
            escape(&self.toggle_label)
        )?;
        write!(
            f,
            "<button class=\"btn btn--ghost\" hx-post=\"/accounts/{}/test\" hx-swap=\"none\" hx-disabled-elt=\"this\">测试</button>",
            self.id
        )?;
        write!(
            f,
            "<button class=\"btn btn--ghost\" hx-post=\"/accounts/{}/refresh\" hx-swap=\"none\" hx-disabled-elt=\"this\">刷新令牌</button>",
            self.id
        )?;
        write!(
            f,
            "<button class=\"btn btn--danger\" hx-post=\"/accounts/{}/delete\" hx-target=\"closest tr\" hx-swap=\"delete\" hx-confirm=\"确定删除这个账号？\" hx-disabled-elt=\"this\">删除</button>",
            self.id
        )?;
        write!(f, "</td></tr>")
    }
}

/// 列表页。
#[derive(Template)]
#[template(path = "accounts/list.html")]
struct AccountsPage {
    nav: Vec<nav::NavItem>,
    rows: Vec<AccountRow>,
    summary: String,
    error: String,
    status: String,
    plan: String,
    keyword: String,
    prev_href: String,
    next_href: String,
    has_prev: bool,
    has_next: bool,
    page_label: String,
}

/// 单行片段。
#[derive(Template)]
#[template(path = "accounts/row.html")]
struct RowFragment {
    row: AccountRow,
}

/// `GET /accounts`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(page(&state, filters).await)
}

async fn page(state: &AppState, filters: Filters) -> AccountsPage {
    let repo = AccountRepo::new(state.db.clone());
    let filter = filters.to_filter();
    let total = repo.count(&filter).await.unwrap_or(-1);
    let (rows, error) = match repo.list(&filter).await {
        Ok(accounts) => (accounts.iter().map(AccountRow::from_account).collect(), String::new()),
        Err(error) => (Vec::new(), format!("读取失败（{error}）")),
    };
    let summary = match state.pool.stats().await {
        Ok(stats) => format!(
            "共 {} 个账号 · 正常 {} · 限流 {} · 异常 {} · 禁用 {} · 正常配额合计 {}",
            stats.total, stats.normal, stats.limited, stats.abnormal, stats.disabled, stats.total_quota
        ),
        Err(error) => format!("账号统计读取失败（{error}）"),
    };
    let offset = filters.offset.max(0);
    let has_next = total >= 0 && offset + PAGE_SIZE < total;
    AccountsPage {
        nav: nav::items("/accounts"),
        rows,
        summary,
        error,
        status: filters.status.clone(),
        plan: filters.plan.clone(),
        keyword: filters.keyword.clone(),
        prev_href: filters.query((offset - PAGE_SIZE).max(0)),
        next_href: filters.query(offset + PAGE_SIZE),
        has_prev: offset > 0,
        has_next,
        page_label: if total >= 0 {
            format!("第 {}–{} 个 / 共 {} 个", offset + 1, offset + PAGE_SIZE.min(total), total)
        } else {
            String::new()
        },
    }
}

/// 行内表单：只带一个状态值。
#[derive(Debug, Deserialize)]
pub struct StatusForm {
    /// 目标状态。
    pub status: String,
}

/// `POST /accounts/{id}/status`
pub async fn toggle_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Form(form): Form<StatusForm>,
) -> Response {
    let request = UpdateAccountRequest {
        id,
        status: Some(form.status.clone()),
        plan: None,
        email: None,
        proxy: None,
        quota: None,
        quota_confirmed: None,
    };
    match crate::routes::accounts::update_account(State(state.clone()), axum::Json(request)).await {
        Ok(_) => row_response(&state, id, "状态已更新", "ok").await,
        Err(error) => toast_only(StatusCode::OK, &format!("状态更新失败：{}", error_message(&error)), "error"),
    }
}

/// `POST /accounts/{id}/delete`
pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Response {
    match crate::routes::accounts::delete_account(State(state.clone()), Path(id)).await {
        Ok(_) => toast_only(StatusCode::OK, "账号已删除", "ok"),
        Err(error) => toast_only(
            StatusCode::OK,
            &format!("删除失败：{}", error_message(&error)),
            "error",
        ),
    }
}

/// `POST /accounts/{id}/test`
pub async fn test_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Response {
    match crate::routes::accounts::test_account(State(state.clone()), Path(id), None).await {
        Ok(result) => {
            let ok = result.status == "success";
            let text = match result.message.as_deref() {
                Some(message) => format!("测试{}：{}", if ok { "通过" } else { "未通过" }, message),
                None => format!(
                    "测试{}：耗时 {} ms{}",
                    if ok { "通过" } else { "未通过" },
                    result.elapsed_ms,
                    result
                        .bytes
                        .map(|bytes| format!("，{} KB", bytes / 1024))
                        .unwrap_or_default()
                ),
            };
            toast_only(StatusCode::OK, &text, if ok { "ok" } else { "error" })
        }
        Err(error) => toast_only(
            StatusCode::OK,
            &format!("测试失败：{}", error_message(&error)),
            "error",
        ),
    }
}

/// `POST /accounts/{id}/refresh`
pub async fn refresh_token(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Response {
    let request = BatchRequest { ids: vec![id] };
    match crate::routes::accounts::refresh_tokens(State(state.clone()), axum::Json(request)).await {
        Ok(batch) => toast_only(
            StatusCode::OK,
            &format!("已开始刷新（{} 个账号），稍后刷新列表查看结果", batch.total),
            "ok",
        ),
        Err(error) => toast_only(
            StatusCode::OK,
            &format!("刷新失败：{}", error_message(&error)),
            "error",
        ),
    }
}

async fn row_response(state: &Arc<AppState>, id: Uuid, message: &str, level: &str) -> Response {
    let repo = AccountRepo::new(state.db.clone());
    match repo.get_by_id(id).await {
        Ok(Some(account)) => {
            let html = RowFragment {
                row: AccountRow::from_account(&account),
            }
            .render();
            match html {
                Ok(html) => Html(format!("{html}{}", toast_oob(message, level))).into_response(),
                Err(error) => toast_only(
                    StatusCode::OK,
                    &format!("渲染失败：{error}"),
                    "error",
                ),
            }
        }
        _ => toast_only(StatusCode::OK, message, level),
    }
}

/// 只回 toast：把提示作为 out-of-band 片段塞回页面，不换主内容。
///
/// 不用 `HX-Trigger` 头：HTTP 头只能是可见 ASCII，中文会被浏览器按 Latin-1 解成乱码。
fn toast_only(status: StatusCode, message: &str, level: &str) -> Response {
    (status, Html(toast_oob(message, level))).into_response()
}

/// 生成 toast 的 OOB 片段。
fn toast_oob(message: &str, level: &str) -> String {
    let class = if level == "error" {
        "toast toast--error"
    } else {
        "toast"
    };
    format!(
        "<div id=\"toasts\" hx-swap-oob=\"beforeend\"><div class=\"{class}\">{}</div></div>",
        escape(message)
    )
}

fn error_message(error: &crate::error::ApiError) -> String {
    format!("{error:?}")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return escape(text);
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{}…", escape(&cut))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape("a<b>&\""), "a&lt;b&gt;&amp;&quot;");
    }

    #[test]
    fn truncates_long_text_with_ellipsis() {
        assert_eq!(truncate("短", 5), "短");
        let long = "0123456789";
        assert_eq!(truncate(long, 4), "0123…");
    }

    #[test]
    fn filter_encodes_query_values() {
        let filters = Filters {
            status: "normal".to_string(),
            plan: String::new(),
            keyword: "a b&c".to_string(),
            offset: 20,
        };
        assert_eq!(
            filters.query(40),
            "?status=normal&plan=&keyword=a%20b%26c&offset=40"
        );
    }

    #[test]
    fn toast_fragment_is_oob_and_escaped() {
        let html = toast_oob("刷新<失败>", "error");
        assert!(html.contains("hx-swap-oob=\"beforeend\""), "{html}");
        assert!(html.contains("toast--error"), "{html}");
        assert!(html.contains("刷新&lt;失败&gt;"), "{html}");
        assert!(!html.contains("<失败>"), "{html}");
    }

    #[test]
    fn page_renders_empty_state_and_active_nav() {
        let page = AccountsPage {
            nav: nav::items("/accounts"),
            rows: Vec::new(),
            summary: "共 0 个账号".to_string(),
            error: String::new(),
            status: String::new(),
            plan: String::new(),
            keyword: String::new(),
            prev_href: String::new(),
            next_href: String::new(),
            has_prev: false,
            has_next: false,
            page_label: String::new(),
        };
        let html = page.render().expect("渲染账号页");
        assert!(html.contains("账号池"), "{html}");
        assert!(html.contains("共 0 个账号"), "{html}");
        assert!(html.contains("没有符合条件的账号"), "{html}");
        assert!(html.contains("is-active"), "账号导航项应为当前页：{html}");
        assert!(html.contains("name=\"keyword\""), "应有筛选输入框：{html}");
    }
}

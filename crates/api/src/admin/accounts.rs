//! 账号池管理页：列表、筛选、分页与行内动作。
//!
//! 行内动作直接复用 `/api/accounts` 的处理函数，避免同一份逻辑写两遍；
//! 每个动作只回一行 `<tr>` 或一个 toast，页面其余部分不重绘。

use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_account::{Account, AccountFilter, AccountRepo, AccountStatus, Plan};
use serde::Deserialize;
use uuid::Uuid;

use super::has_session;
use super::nav;
use super::toast::oob as toast_oob;
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
    /// 一次性提示（从重定向带回来）。
    #[serde(default)]
    pub flash: String,
    /// 要跟踪的后台操作 ID。
    #[serde(default)]
    pub op: String,
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

pub(super) fn encode(text: &str) -> String {
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

/// 只带一个 flash 的查询串。
#[derive(Debug, Default, Deserialize)]
pub struct FlashQuery {
    /// 一次性提示。
    #[serde(default)]
    pub flash: String,
}

/// 导入表单。
#[derive(Debug, Deserialize)]
pub struct ImportForm {
    /// 粘贴的 token / JSON 文本。
    #[serde(default)]
    pub text: String,
}

/// `POST /accounts/import`
pub async fn import_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ImportForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let text = form.text.trim();
    if text.is_empty() {
        return redirect_with_flash("/accounts", "没有内容可导入");
    }
    let accounts = match gpt2apirs_account::parse_any(text) {
        Ok(accounts) if !accounts.is_empty() => accounts,
        Ok(_) => return redirect_with_flash("/accounts", "没有解析出账号，请检查 access_token"),
        Err(error) => return redirect_with_flash("/accounts", &format!("解析失败：{error}")),
    };
    match state.pool.repo().upsert_many(&accounts).await {
        Ok(report) => {
            let _ = state.pool.reload().await;
            let message = format!(
                "导入完成：新增 {} 个，合并 {} 个，空行跳过 {} 个",
                report.inserted.len(),
                report.merged.len(),
                report.skipped_blank
            );
            redirect_with_flash("/accounts", &message)
        }
        Err(error) => redirect_with_flash("/accounts", &format!("写入失败：{error}")),
    }
}

/// 批量动作表单：ids 可重复，action 取 sync/refresh/enable/disable/delete/export。
#[derive(Debug, Deserialize)]
pub struct BatchForm {
    /// 选中的账号。
    #[serde(default)]
    pub ids: Vec<Uuid>,
    /// 动作名。
    pub action: String,
}

/// `POST /accounts/batch`
pub async fn batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BatchForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let ids = form.ids.clone();
    match form.action.as_str() {
        "sync" => match crate::routes::accounts::sync_accounts(
            State(state.clone()),
            axum::Json(BatchRequest { ids }),
        )
        .await
        {
            Ok(batch) => Redirect::to(&format!("/accounts?op={}", batch.operation_id)).into_response(),
            Err(error) => redirect_with_flash(
                "/accounts",
                &format!("同步失败：{}", error_message(&error)),
            ),
        },
        "refresh" => match crate::routes::accounts::refresh_tokens(
            State(state.clone()),
            axum::Json(BatchRequest { ids }),
        )
        .await
        {
            Ok(batch) => Redirect::to(&format!("/accounts?op={}", batch.operation_id)).into_response(),
            Err(error) => redirect_with_flash(
                "/accounts",
                &format!("刷新失败：{}", error_message(&error)),
            ),
        },
        "enable" | "disable" => {
            let target = if form.action == "enable" { "normal" } else { "disabled" };
            let label = if target == "normal" { "启用" } else { "禁用" };
            let mut done = 0;
            let mut failed = 0;
            for id in ids {
                let request = UpdateAccountRequest {
                    id,
                    status: Some(target.to_string()),
                    plan: None,
                    email: None,
                    proxy: None,
                    quota: None,
                    quota_confirmed: None,
                };
                match crate::routes::accounts::update_account(
                    State(state.clone()),
                    axum::Json(request),
                )
                .await
                {
                    Ok(_) => done += 1,
                    Err(_) => failed += 1,
                }
            }
            redirect_with_flash(
                "/accounts",
                &format!("批量{label}完成：成功 {done} 个，失败 {failed} 个"),
            )
        }
        "delete" => match crate::routes::accounts::delete_accounts(
            State(state.clone()),
            axum::Json(crate::routes::accounts::DeleteAccountsRequest { ids }),
        )
        .await
        {
            Ok(value) => {
                let removed = value.0.get("removed").and_then(|v| v.as_u64()).unwrap_or(0);
                redirect_with_flash("/accounts", &format!("已删除 {removed} 个账号"))
            }
            Err(error) => redirect_with_flash(
                "/accounts",
                &format!("删除失败：{}", error_message(&error)),
            ),
        },
        "export" => export_attachment(&state, ids).await,
        other => redirect_with_flash("/accounts", &format!("未知动作 {other}")),
    }
}

/// 导出为附件下载。
async fn export_attachment(state: &Arc<AppState>, ids: Vec<Uuid>) -> Response {
    let request = crate::routes::accounts::ExportRequest {
        ids,
        format: Some("txt".to_string()),
    };
    match crate::routes::accounts::export_accounts(State(state.clone()), axum::Json(request)).await {
        Ok(value) => {
            let content = value
                .0
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            (
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"accounts.txt\"",
                    ),
                ],
                content,
            )
                .into_response()
        }
        Err(error) => redirect_with_flash(
            "/accounts",
            &format!("导出失败：{}", error_message(&error)),
        ),
    }
}

/// 跳回列表并带一条提示。
fn redirect_with_flash(base: &str, message: &str) -> Response {
    let encoded = encode(message);
    let separator = if base.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{base}{separator}flash={encoded}")).into_response()
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
            "<tr id=\"account-{}\"><td class=\"pick\"><input type=\"checkbox\" name=\"ids\" value=\"{}\"></td>",
            self.id,
            self.id
        )?;
        write!(f, "<td>{}</td>", escape(&self.email))?;
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
        write!(
            f,
            "<a class=\"btn btn--ghost\" href=\"/accounts/{}/edit\">编辑</a>",
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
    flash: String,
    op: String,
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
        flash: filters.flash.clone(),
        op: filters.op.clone(),
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

/// 进度片段模板。
#[derive(Template)]
#[template(path = "accounts/operation.html")]
struct OperationPage {
    id: String,
    label: String,
    processed: String,
    total: String,
    percent: u32,
    succeeded: String,
    failed: String,
    done: bool,
    errors: Vec<String>,
}

/// `GET /accounts/operations/{id}`：返回进度片段，未完成时自己继续轮询。
pub async fn operation_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let Some(progress) = state.progress.get(id) else {
        return render(OperationPage {
            id: id.to_string(),
            label: "操作不存在或已过期".to_string(),
            processed: String::new(),
            total: String::new(),
            percent: 0,
            succeeded: String::new(),
            failed: String::new(),
            done: true,
            errors: Vec::new(),
        });
    };
    let total = progress.total.max(1);
    let percent = ((progress.processed.min(total) as f64 / total as f64) * 100.0).round() as u32;
    let errors = progress
        .errors
        .iter()
        .take(5)
        .map(|error| format!("{}：{}", error.label, error.message))
        .collect();
    render(OperationPage {
        id: id.to_string(),
        label: match progress.kind {
            gpt2apirs_account::OperationKind::Sync => "同步".to_string(),
            gpt2apirs_account::OperationKind::RefreshToken => "刷新令牌".to_string(),
            gpt2apirs_account::OperationKind::Test => "账号测试".to_string(),
            gpt2apirs_account::OperationKind::Import => "导入".to_string(),
        },
        processed: progress.processed.to_string(),
        total: progress.total.to_string(),
        percent,
        succeeded: progress.succeeded.to_string(),
        failed: progress.failed.to_string(),
        done: progress.done,
        errors,
    })
}

/// 发起 OAuth 的表单。
#[derive(Debug, Deserialize)]
pub struct OAuthStartForm {
    /// 邮箱提示。
    #[serde(default)]
    pub email_hint: String,
}

/// OAuth 授权页。
#[derive(Template)]
#[template(path = "accounts/oauth.html")]
struct OAuthPage {
    nav: Vec<nav::NavItem>,
    session_id: String,
    authorize_url: String,
    email_hint: String,
}

/// `POST /accounts/oauth/start`
pub async fn oauth_start_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<OAuthStartForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let hint = form.email_hint.trim();
    let start = gpt2apirs_account::oauth::start(
        &state.oauth,
        Some(hint).filter(|value| !value.is_empty()),
    );
    render(OAuthPage {
        nav: nav::items("/accounts"),
        session_id: start.session_id.to_string(),
        authorize_url: start.authorize_url,
        email_hint: form.email_hint,
    })
}

/// 完成 OAuth 的表单。
#[derive(Debug, Deserialize)]
pub struct OAuthFinishForm {
    /// 会话 ID。
    pub session_id: String,
    /// 回调地址或裸 code。
    #[serde(default)]
    pub callback: String,
}

/// `POST /accounts/oauth/finish`
pub async fn oauth_finish_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<OAuthFinishForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let callback = form.callback.trim().to_string();
    if callback.is_empty() {
        return redirect_with_flash("/accounts", "请粘贴回调地址或 code");
    }
    let session_id = match form.session_id.trim().parse::<Uuid>() {
        Ok(value) => value,
        Err(_) => return redirect_with_flash("/accounts", "会话 ID 不合法，请重新发起授权"),
    };
    let request = crate::routes::accounts::OAuthFinishRequest {
        session_id,
        callback,
    };
    match crate::routes::accounts::oauth_finish(State(state.clone()), axum::Json(request)).await {
        Ok(response) => {
            let _ = state.pool.reload().await;
            redirect_with_flash("/accounts", &format!("OAuth 导入完成：新增 {} 个", response.added))
        }
        Err(error) => redirect_with_flash(
            "/accounts",
            &format!("OAuth 导入失败：{}", error_message(&error)),
        ),
    }
}

/// 编辑页。
#[derive(Template)]
#[template(path = "accounts/edit.html")]
struct EditPage {
    nav: Vec<nav::NavItem>,
    id: String,
    email: String,
    proxy: String,
    quota: String,
    plan: String,
    status: String,
    flash: String,
}

/// 编辑表单。
#[derive(Debug, Deserialize)]
pub struct EditForm {
    /// 邮箱。
    #[serde(default)]
    pub email: String,
    /// 代理串；留空表示走全局出口。
    #[serde(default)]
    pub proxy: String,
    /// 剩余配额。
    #[serde(default)]
    pub quota: String,
    /// 套餐。
    #[serde(default)]
    pub plan: String,
    /// 状态。
    #[serde(default)]
    pub status: String,
}

/// `GET /accounts/{id}/edit`
pub async fn edit_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<FlashQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let repo = AccountRepo::new(state.db.clone());
    match repo.get_by_id(id).await {
        Ok(Some(account)) => render(EditPage {
            nav: nav::items("/accounts"),
            id: account.id.to_string(),
            email: account.email.clone().unwrap_or_default(),
            proxy: account.proxy.clone(),
            quota: account.quota.to_string(),
            plan: account.plan.as_str().to_string(),
            status: account.status.as_str().to_string(),
            flash: query.flash.clone(),
        }),
        Ok(None) => redirect_with_flash("/accounts", "账号不存在"),
        Err(error) => redirect_with_flash("/accounts", &format!("读取失败：{error}")),
    }
}

/// `POST /accounts/{id}/edit`
pub async fn edit_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<EditForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let trimmed = |value: &str| -> Option<String> {
        Some(value.trim().to_string()).filter(|value| !value.is_empty())
    };
    let quota = match form.quota.trim() {
        "" => None,
        raw => match raw.parse::<i32>() {
            Ok(value) => Some(value),
            Err(_) => return redirect_with_flash("/accounts", "配额必须是整数"),
        },
    };
    let request = UpdateAccountRequest {
        id,
        status: trimmed(&form.status),
        plan: trimmed(&form.plan),
        email: Some(form.email.trim().to_string()),
        proxy: Some(form.proxy.trim().to_string()),
        quota,
        quota_confirmed: quota.map(|_| true),
    };
    match crate::routes::accounts::update_account(State(state.clone()), axum::Json(request)).await {
        Ok(_) => redirect_with_flash("/accounts", "账号已更新"),
        Err(error) => redirect_with_flash(&format!("/accounts/{id}/edit"), &format!("保存失败：{}", error_message(&error))),
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
            flash: String::new(),
            op: String::new(),
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
    fn operation_fragment_polls_until_done() {
        let running = OperationPage {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            label: "同步".to_string(),
            processed: "3".to_string(),
            total: "10".to_string(),
            percent: 30,
            succeeded: "3".to_string(),
            failed: "0".to_string(),
            done: false,
            errors: Vec::new(),
        };
        let html = running.render().expect("渲染进度");
        assert!(html.contains("hx-trigger=\"every 2s\""), "未完成时继续轮询：{html}");
        assert!(html.contains("width: 30%"), "{html}");

        let finished = OperationPage {
            done: true,
            errors: vec!["a@b：token 失效".to_string()],
            ..running
        };
        let html = finished.render().expect("渲染完成");
        assert!(!html.contains("hx-trigger=\"every 2s\""), "完成后停止轮询：{html}");
        assert!(html.contains("a@b：token 失效"), "{html}");
        assert!(html.contains("刷新列表"), "{html}");
    }

    #[test]
    fn oauth_page_shows_authorize_url() {
        let page = OAuthPage {
            nav: nav::items("/accounts"),
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            authorize_url: "https://auth.openai.com/oauth/authorize?x=1".to_string(),
            email_hint: "me@example.com".to_string(),
        };
        let html = page.render().expect("渲染授权页");
        assert!(html.contains("auth.openai.com"), "{html}");
        assert!(html.contains("name=\"callback\""), "{html}");
        assert!(html.contains("me@example.com"), "{html}");
    }

    #[test]
    fn edit_page_renders_fields_and_flash() {
        let page = EditPage {
            nav: nav::items("/accounts"),
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            email: "a@b.com".to_string(),
            proxy: "http://127.0.0.1:17899".to_string(),
            quota: "12".to_string(),
            plan: "free".to_string(),
            status: "normal".to_string(),
            flash: "保存失败：配额必须是整数".to_string(),
        };
        let html = page.render().expect("渲染编辑页");
        assert!(html.contains("value=\"a@b.com\""), "{html}");
        assert!(html.contains("保存失败：配额必须是整数"), "{html}");
        assert!(html.contains("action=\"/accounts/11111111-1111-1111-1111-111111111111/edit\""), "{html}");
    }

    #[test]
    fn page_renders_empty_state_and_active_nav() {
        let page = AccountsPage {
            nav: nav::items("/accounts"),
            rows: Vec::new(),
            summary: "共 0 个账号".to_string(),
            flash: String::new(),
            op: String::new(),
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

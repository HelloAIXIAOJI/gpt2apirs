//! 账号池管理页：列表、筛选、分页与行内动作。
//!
//! 行内动作直接复用 `/api/accounts` 的处理函数，避免同一份逻辑写两遍；
//! 每个动作只回一行 `<tr>` 或一个 toast，页面其余部分不重绘。

use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use chrono::{DateTime, Utc};
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
    /// 页码，从 1 开始。
    #[serde(default)]
    pub page: i64,
    /// 一次性提示（从重定向带回来）。
    #[serde(default)]
    pub flash: String,
    /// 要跟踪的后台操作 ID。
    #[serde(default)]
    pub op: String,
    /// 批量作用域（只给作用域预估用）。
    #[serde(default)]
    pub scope: String,
}

impl Filters {
    fn page_prefix(&self) -> String {
        format!(
            "?status={}&plan={}&keyword={}&page=",
            encode(&self.status),
            encode(&self.plan),
            encode(&self.keyword)
        )
    }

    fn to_filter(&self, offset: i64, limit: i64) -> AccountFilter {
        AccountFilter {
            status: AccountStatus::parse(self.status.trim()),
            plan: if self.plan.trim().is_empty() {
                None
            } else {
                Some(Plan::parse(self.plan.trim()))
            },
            source: None,
            keyword: Some(self.keyword.trim().to_string()).filter(|value| !value.is_empty()),
            offset,
            limit,
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
    /// 导入到哪个桶；空 = 默认桶。
    #[serde(default)]
    pub bucket_id: String,
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
    let mut accounts = match gpt2apirs_account::parse_any(text) {
        Ok(accounts) if !accounts.is_empty() => accounts,
        Ok(_) => return redirect_with_flash("/accounts", "没有解析出账号，请检查 access_token"),
        Err(error) => return redirect_with_flash("/accounts", &format!("解析失败：{error}")),
    };
    // 页面选定的桶：留空就交给 SQL 的 coalesce 落到默认桶
    let bucket = form.bucket_id.trim().parse::<Uuid>().ok();
    if bucket.is_some() {
        for account in &mut accounts {
            account.bucket_id = bucket;
        }
    }
    // 与 API 导入共用一个入口：否则「导入后清理无效号」只在 API 那条路生效
    match crate::routes::accounts::upsert_and_prune(&state, &accounts).await {
        Ok((report, pruned)) => {
            let mut message = format!(
                "导入完成：新增 {} 个，合并 {} 个，空行跳过 {} 个",
                report.inserted.len(),
                report.merged.len(),
                report.skipped_blank
            );
            if pruned > 0 {
                message.push_str(&format!("，已清理无效号 {pruned} 个"));
            }
            redirect_with_flash("/accounts", &message)
        }
        Err(error) => redirect_with_flash("/accounts", &format!("写入失败：{}", error.error)),
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
    /// 作用域：`selected`（勾选的）/ `filtered`（按当前筛选）/ `all`（全部）。
    #[serde(default)]
    pub scope: String,
    /// 当前筛选（作用域为 filtered 时用）。
    #[serde(default)]
    pub status: String,
    /// 当前筛选：套餐。
    #[serde(default)]
    pub plan: String,
    /// 当前筛选：关键词。
    #[serde(default)]
    pub keyword: String,
}

impl BatchForm {
    fn filters(&self) -> Filters {
        Filters {
            status: self.status.clone(),
            plan: self.plan.clone(),
            keyword: self.keyword.clone(),
            ..Filters::default()
        }
    }
}

/// 按作用域解析真正要操作的账号。
async fn scope_ids(
    state: &AppState,
    form: &BatchForm,
    forced_status: Option<&str>,
) -> Result<Vec<Uuid>, String> {
    let repo = AccountRepo::new(state.db.clone());
    match form.scope.as_str() {
        "all" => Ok(repo
            .list_all()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|account| match forced_status {
                Some(status) => account.status.as_str() == status,
                None => true,
            })
            .map(|account| account.id)
            .collect()),
        "filtered" => {
            let mut filters = form.filters();
            if let Some(status) = forced_status {
                filters.status = status.to_string();
            }
            // 显式大上限：作用域是「全部符合条件的」，不能被默认页大小截断
            let filter = filters.to_filter(0, 100_000);
            Ok(repo
                .list(&filter)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|account| account.id)
                .collect())
        }
        _ => Ok(form.ids.clone()),
    }
}

/// `POST /accounts/batch`
pub async fn batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let mut form = batch_form(&body);
    // 两个一键动作先归一成普通动作 + 固定作用域
    match form.action.as_str() {
        "sync_all" => {
            form.action = "sync".to_string();
            form.scope = "all".to_string();
        }
        "remove_abnormal" => {
            form.action = "delete".to_string();
            if form.scope.is_empty() || form.scope == "selected" {
                form.scope = "filtered".to_string();
            }
            form.status = "abnormal".to_string();
        }
        _ => {}
    }
    if form.scope.is_empty() {
        form.scope = "selected".to_string();
    }
    let forced_status = (form.action == "delete" && form.status == "abnormal").then_some("abnormal");
    let ids = match scope_ids(&state, &form, forced_status).await {
        Ok(ids) => ids,
        Err(error) => return redirect_with_flash("/accounts", &format!("解析作用域失败：{error}")),
    };
    if ids.is_empty() && matches!(form.action.as_str(), "delete" | "enable" | "disable" | "export") {
        return redirect_with_flash("/accounts", "没有符合条件的目标账号");
    }
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
                    bucket_id: None,
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

/// 从原始表单里取出动作与 id 列表。
///
/// `serde_urlencoded` 不支持重复键，所以批量表单自己解析。
fn batch_form(body: &str) -> BatchForm {
    let mut form = BatchForm {
        ids: Vec::new(),
        action: String::new(),
        scope: String::new(),
        status: String::new(),
        plan: String::new(),
        keyword: String::new(),
    };
    for (key, value) in super::parse_form_pairs(body) {
        match key.as_str() {
            "ids" => {
                if let Ok(id) = value.parse::<Uuid>() {
                    form.ids.push(id);
                }
            }
            "action" => form.action = value,
            "scope" => form.scope = value,
            "status" => form.status = value,
            "plan" => form.plan = value,
            "keyword" => form.keyword = value,
            _ => {}
        }
    }
    form
}

/// 把剩余时间说成人话（列表里只放得下一小段）。
fn humanize_duration(left: chrono::Duration) -> String {
    let minutes = left.num_minutes();
    if minutes < 1 {
        return "不到 1 分钟".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} 分钟");
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if hours < 24 {
        return if rest == 0 {
            format!("{hours} 小时")
        } else {
            format!("{hours} 小时 {rest} 分")
        };
    }
    format!("{} 天 {} 小时", hours / 24, hours % 24)
}

/// `GET /accounts/bulk/preview` —— 批量操作会命中多少账号（页面上的预估）。
///
/// 只回一句 HTML 片段：作用域 + 当前筛选一起决定命中数，切换下拉时 htmx 直接换掉它。
pub async fn bulk_preview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let scope = if filters.scope.is_empty() {
        "selected".to_string()
    } else {
        filters.scope.clone()
    };
    let repo = AccountRepo::new(state.db.clone());
    let count = match scope.as_str() {
        "all" => repo.list_all().await.map(|items| items.len()),
        _ => repo.count(&filters.to_filter(0, 0)).await.map(|n| n.max(0) as usize),
    };
    let text = match count {
        Ok(n) => match scope.as_str() {
            "all" => format!("将影响全部 {n} 个账号"),
            "filtered" => format!("将影响当前筛选的 {n} 个账号"),
            _ => "只影响勾选的账号".to_string(),
        },
        Err(error) => format!("统计失败（{error}）"),
    };
    Html(format!(
        "<span id=\"scope-hint\" class=\"muted\">{}</span>",
        escape(&text)
    ))
    .into_response()
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
    /// 配额数字（不限量时是「不限」）。
    quota: String,
    /// 配额语义：可用 / 已耗尽 / 未知 / 不限。
    quota_state: String,
    /// 配额语义的样式后缀。
    quota_tone: String,
    /// 距配额重置的倒计时（没有就是空串）。
    reset_hint: String,
    counters: String,
    /// 所属桶名（账号归桶，桶再管出口）。
    bucket: String,
    /// 生效出口；空表示跟随全局。
    proxy: String,
    /// 状态原因的简短说法。
    reason: String,
    /// 上游原始错误（悬停看）。
    last_error: String,
    /// access token 状态（有效 / 即将过期 / 无效）与悬停说明。
    access_label: String,
    access_tone: String,
    access_hint: String,
    /// refresh token 状态。
    refresh_label: String,
    refresh_tone: String,
    toggle_label: String,
    toggle_status: String,
}

impl AccountRow {
    /// 构造一行。`bucket_name` 由调用方按桶表解析后传入。
    fn from_account(account: &Account, now: DateTime<Utc>, bucket_name: &str) -> Self {
        let disabled = account.status == AccountStatus::Disabled;
        // 凭据状态由 token 本身算（不落库），和 /api/accounts/{id} 用同一个投影
        let credential = gpt2apirs_account::project(
            &account.access_token,
            account.refresh_token.as_deref().unwrap_or(""),
            account.status == AccountStatus::Abnormal,
            false,
            now,
        );
        let quota_state = account.quota_state();
        let reset_hint = account
            .quota_reset_in(now)
            .map(|left| format!("{}后重置", humanize_duration(left)))
            .unwrap_or_default();
        let (reason, raw_error) = account.status_reason();
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
            quota: if account.quota_unlimited {
                "不限".to_string()
            } else if account.quota_confirmed {
                account.quota.to_string()
            } else {
                "?".to_string()
            },
            quota_state: quota_state.label().to_string(),
            quota_tone: match quota_state {
                gpt2apirs_account::QuotaState::Unlimited => "info".to_string(),
                gpt2apirs_account::QuotaState::Available => "ok".to_string(),
                gpt2apirs_account::QuotaState::Exhausted => "warn".to_string(),
                gpt2apirs_account::QuotaState::Unknown => "muted".to_string(),
            },
            reset_hint,
            counters: format!("{} / {}", account.success_count, account.fail_count),
            // 桶名由调用方传进来：这里只拿得到账号，桶表要单独查一次
            bucket: bucket_name.to_string(),
            proxy: if account.proxy.trim().is_empty() {
                "跟随全局".to_string()
            } else {
                account.proxy.clone()
            },
            reason: reason.to_string(),
            last_error: raw_error.to_string(),
            access_label: credential.access.label().to_string(),
            access_tone: match credential.access {
                gpt2apirs_account::AccessTokenStatus::Valid => "ok".to_string(),
                gpt2apirs_account::AccessTokenStatus::Expiring => "warn".to_string(),
                gpt2apirs_account::AccessTokenStatus::Invalid => "danger".to_string(),
            },
            access_hint: match credential.expires_at {
                Some(exp) => format!("到期 {}", exp.format("%Y-%m-%d %H:%M UTC")),
                None => "读不出过期时间".to_string(),
            },
            refresh_label: credential.refresh.label().to_string(),
            refresh_tone: match credential.refresh {
                gpt2apirs_account::RefreshTokenStatus::Valid => "ok".to_string(),
                gpt2apirs_account::RefreshTokenStatus::Missing => "muted".to_string(),
                gpt2apirs_account::RefreshTokenStatus::Invalid => "danger".to_string(),
            },
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
        // 凭据：AT 状态 + RT 状态，悬停看到期时间
        write!(
            f,
            "<td class=\"cred\"><span class=\"badge badge--{}\" title=\"{}\">AT {}</span> <span class=\"badge badge--{}\">RT {}</span></td>",
            escape(&self.access_tone),
            escape(&self.access_hint),
            escape(&self.access_label),
            escape(&self.refresh_tone),
            escape(&self.refresh_label)
        )?;
        // 配额：语义 + 数字 + 重置倒计时
        write!(
            f,
            "<td class=\"num\"><span class=\"badge badge--{}\">{}</span> {}",
            escape(&self.quota_tone),
            escape(&self.quota_state),
            escape(&self.quota)
        )?;
        if !self.reset_hint.is_empty() {
            write!(
                f,
                " <span class=\"muted\" title=\"{}\">{}</span>",
                escape(&self.reset_hint),
                escape(&self.reset_hint)
            )?;
        }
        write!(f, "</td>")?;
        write!(f, "<td class=\"num\">{}</td>", escape(&self.counters))?;
        // 一列同时给出「在哪个桶」和「走什么出口」：想改出口就去改桶
        write!(
            f,
            "<td title=\"出口：{}\" class=\"muted\">{} · {}</td>",
            escape(&self.proxy),
            escape(&self.bucket),
            escape(&self.proxy)
        )?;
        // 状态原因：短因一列，原始错误悬停看
        write!(
            f,
            "<td class=\"muted\" title=\"{}\">{}</td>",
            escape(&self.last_error),
            escape(&self.reason)
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
    pager: super::pagination::PageWindow,
    page_label: String,
    /// 导入表单的桶下拉。
    buckets: Vec<BucketOption>,
}

/// 导入表单里的桶下拉项。
pub struct BucketOption {
    /// 桶 ID。
    pub id: String,
    /// 桶名。
    pub name: String,
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

/// 桶 ID → 桶名。查不到就显示短 ID，别让整页因为一张表读不到而空白。
async fn bucket_names(state: &AppState) -> std::collections::HashMap<Uuid, String> {
    match gpt2apirs_storage::repo::BucketRepo::new(state.db.clone()).list().await {
        Ok(rows) => rows.into_iter().map(|row| (row.id, row.name)).collect(),
        Err(error) => {
            tracing::warn!(%error, "读取桶列表失败");
            std::collections::HashMap::new()
        }
    }
}

fn bucket_label(names: &std::collections::HashMap<Uuid, String>, id: Uuid) -> String {
    names
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("已删除({})", &id.to_string()[..8]))
}

async fn page(state: &AppState, filters: Filters) -> AccountsPage {
    let repo = AccountRepo::new(state.db.clone());
    let size = super::pagination::page_size(&state.settings);
    let page = filters.page.max(1);
    let filter = filters.to_filter((page - 1) * size, size);
    let total = repo.count(&filter).await.unwrap_or(-1);
    let (rows, error) = match repo.list(&filter).await {
        Ok(accounts) => {
            let now = chrono::Utc::now();
            let names = bucket_names(state).await;
            (
                accounts
                    .iter()
                    .map(|a| AccountRow::from_account(a, now, &bucket_label(&names, a.bucket_id)))
                    .collect(),
                String::new(),
            )
        }
        Err(error) => (Vec::new(), format!("读取失败（{error}）")),
    };
    let summary = match state.pool.stats().await {
        Ok(stats) => format!(
            "共 {} 个账号 · 正常 {} · 限流 {} · 异常 {} · 禁用 {} · 正常配额合计 {}",
            stats.total, stats.normal, stats.limited, stats.abnormal, stats.disabled, stats.total_quota
        ),
        Err(error) => format!("账号统计读取失败（{error}）"),
    };
    let pager = super::pagination::window(total.max(0), page, size, &filters.page_prefix());
    let pager = if total >= 0 {
        pager
    } else {
        super::pagination::PageWindow {
            visible: false,
            summary: String::new(),
            ..pager
        }
    };
    let buckets = bucket_names(&state).await
        .into_iter()
        .map(|(id, name)| BucketOption {
            id: id.to_string(),
            name,
        })
        .collect();
    AccountsPage {
        nav: nav::items("/accounts"),
        rows,
        buckets,
        summary,
        flash: filters.flash.clone(),
        op: filters.op.clone(),
        error,
        status: filters.status.clone(),
        plan: filters.plan.clone(),
        keyword: filters.keyword.clone(),
        page_label: pager.summary.clone(),
        pager,
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
        bucket_id: None,
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
                row: AccountRow::from_account(
                    &account,
                    chrono::Utc::now(),
                    &bucket_label(&bucket_names(state).await, account.bucket_id),
                ),
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
    /// 换桶（账号的出口跟着桶走）。
    #[serde(default)]
    pub bucket_id: String,
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
        bucket_id: form
            .bucket_id
            .trim()
            .parse()
            .ok()
            .filter(|_| !form.bucket_id.trim().is_empty()),
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

    /// 造一个指定过期时间的 AT（只需 payload 里的 exp，不验签）。
    fn jwt_expiring_in(seconds: i64) -> String {
        use base64::Engine as _;
        let payload = serde_json::json!({ "exp": (Utc::now().timestamp() + seconds) });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.to_string().as_bytes());
        format!("header.{encoded}.sig")
    }

    fn account_for_row(quota: i32, unlimited: bool, token_secs: i64) -> Account {
        let now = Utc::now();
        Account {
            id: Uuid::new_v4(),
            bucket_id: gpt2apirs_storage::repo::DEFAULT_BUCKET_ID,
            access_token: jwt_expiring_in(token_secs),
            refresh_token: Some("rt".to_string()),
            id_token: None,
            email: Some("row@test.local".to_string()),
            upstream_account_id: None,
            plan: Plan::Plus,
            source: gpt2apirs_account::Source::Web,
            status: AccountStatus::Normal,
            quota,
            quota_confirmed: true,
            quota_unlimited: unlimited,
            restore_at: Some(now + chrono::Duration::minutes(45)),
            proxy: String::new(),
            success_count: 7,
            fail_count: 1,
            invalid_count: 0,
            last_used_at: None,
            last_invalid_at: None,
            last_error: None,
            last_error_at: None,
            last_checked_at: None,
            last_refresh_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn row_shows_credentials_quota_semantics_and_countdown() {
        let now = Utc::now();

        // 即将过期 + 不限量 + 有重置时间
        let row = AccountRow::from_account(&account_for_row(0, true, 3600), now, "默认").to_string();
        assert!(row.contains("AT 即将过期"), "凭据要显示到期风险：{row}");
        assert!(row.contains("RT 有效"), "{row}");
        assert!(
            row.contains("badge--info\">不限</span> 不限"),
            "不限量要在状态和数字两处都说清楚：{row}"
        );
        assert!(row.contains("后重置"), "要给重置倒计时：{row}");

        // 一张不剩：语义是「已耗尽」
        let mut exhausted = account_for_row(0, false, 7200);
        exhausted.restore_at = None;
        let row = AccountRow::from_account(&exhausted, now, "默认").to_string();
        assert!(row.contains("已耗尽"), "{row}");
        assert!(!row.contains("后重置"), "没有重置时间就别显示倒计时：{row}");

        // AT 过期：凭据列要说无效
        let row = AccountRow::from_account(&account_for_row(3, false, -3600), now, "默认").to_string();
        assert!(row.contains("AT 无效"), "{row}");
    }

    #[test]
    fn reset_countdown_reads_like_a_sentence() {
        use chrono::Duration;
        assert_eq!(humanize_duration(Duration::seconds(30)), "不到 1 分钟");
        assert_eq!(humanize_duration(Duration::minutes(45)), "45 分钟");
        assert_eq!(humanize_duration(Duration::hours(3)), "3 小时");
        assert_eq!(humanize_duration(Duration::minutes(200)), "3 小时 20 分");
        assert_eq!(humanize_duration(Duration::hours(50)), "2 天 2 小时");
    }

    #[test]
    fn batch_form_reads_scope_and_filters() {
        let form = batch_form("action=delete&scope=filtered&status=abnormal&plan=plus&keyword=ab");
        assert_eq!(form.action, "delete");
        assert_eq!(form.scope, "filtered");
        let filters = form.filters();
        assert_eq!(filters.status, "abnormal");
        assert_eq!(filters.plan, "plus");
        assert_eq!(filters.keyword, "ab");
    }

    #[test]
    fn filter_encodes_query_values() {
        let filters = Filters {
            status: "normal".to_string(),
            plan: String::new(),
            keyword: "a b&c".to_string(),
            page: 2,
            flash: String::new(),
            op: String::new(),
            scope: String::new(),
        };
        assert_eq!(
            filters.page_prefix(),
            "?status=normal&plan=&keyword=a%20b%26c&page="
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
            pager: crate::admin::pagination::window(0, 1, 50, "/accounts?page="),
            page_label: String::new(),
            buckets: vec![BucketOption {
                id: gpt2apirs_storage::repo::DEFAULT_BUCKET_ID.to_string(),
                name: "默认".to_string(),
            }],
        };
        let html = page.render().expect("渲染账号页");
        assert!(html.contains("账号池"), "{html}");
        assert!(html.contains("共 0 个账号"), "{html}");
        assert!(html.contains("没有符合条件的账号"), "{html}");
        assert!(html.contains("is-active"), "账号导航项应为当前页：{html}");
        assert!(html.contains("name=\"keyword\""), "应有筛选输入框：{html}");
    }
}

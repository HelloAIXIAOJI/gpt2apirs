//! 日志页：调用记录筛选、概览指标与明细分页。
//!
//! 指标与明细都来自 `call_records` 一张表：概览走 summary，明细走 list；
//! 读取失败时在页面上写明原因，不把错误吞掉。

use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use gpt2apirs_storage::repo::{CallRecordQuery, CallRecordRepo, CallRecordRow};
use serde::Deserialize;

use super::card::StatCard;
use super::has_session;
use super::nav;
use crate::state::AppState;

/// 每页条数。

/// 筛选参数。
#[derive(Debug, Deserialize)]
pub struct Filters {
    /// 时间范围（小时），默认 24。
    #[serde(default)]
    pub hours: i64,
    /// 状态：空表示全部，ok 表示 2xx，fail 表示非 2xx。
    #[serde(default)]
    pub status: String,
    /// 路径前缀。
    #[serde(default)]
    pub path: String,
    /// 错误码。
    #[serde(default)]
    pub code: String,
    /// 页码，从 1 开始。
    #[serde(default)]
    pub page: i64,
    /// 上一动作删掉了几条（只用于回显，不参与筛选）。
    #[serde(default)]
    pub deleted: i64,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            hours: 24,
            status: String::new(),
            path: String::new(),
            code: String::new(),
            page: 1,
            deleted: 0,
        }
    }
}

impl Filters {
    fn hours(&self) -> i64 {
        // 没带参数时 serde 给的是 0，默认窗口按 24 小时算。
        let raw = if self.hours <= 0 { 24 } else { self.hours };
        raw.clamp(1, 24 * 30)
    }

    /// 删除表单的隐藏字段：让 POST 之后还能回到同一筛选。
    fn hidden_fields(&self) -> String {
        [
            ("hours", self.hours().to_string()),
            ("status", self.status.clone()),
            ("path", self.path.clone()),
            ("code", self.code.clone()),
        ]
        .iter()
        .map(|(name, value)| {
            format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                escape(name),
                escape(value)
            )
        })
        .collect()
    }

    /// 只带筛选条件的查询串（不含页码）：详情页返回、删除后跳转都用它。
    fn back_query(&self) -> String {
        format!(
            "?hours={}&status={}&path={}&code={}",
            self.hours(),
            super::accounts::encode(&self.status),
            super::accounts::encode(&self.path),
            super::accounts::encode(&self.code)
        )
    }

    /// 只带筛选条件的前缀，页码由分页控件追加。
    fn page_prefix(&self) -> String {
        format!(
            "?hours={}&status={}&path={}&code={}&page=",
            self.hours(),
            super::accounts::encode(&self.status),
            super::accounts::encode(&self.path),
            super::accounts::encode(&self.code)
        )
    }

    fn to_query(&self) -> CallRecordQuery {
        let status_range = match self.status.trim() {
            "ok" => Some((200, 299)),
            "fail" => Some((400, 599)),
            _ => None,
        };
        CallRecordQuery {
            status_range,
            path_prefix: Some(self.path.trim().to_string()).filter(|value| !value.is_empty()),
            account_id: None,
            error_code: Some(self.code.trim().to_string()).filter(|value| !value.is_empty()),
            since: Some(Utc::now() - Duration::hours(self.hours())),
            limit: 200,
            offset: 0,
        }
    }
}

/// 表格里的一行。
pub struct LogRow {
    id: i64,
    back: String,
    /// 删除表单的隐藏字段（把当前筛选原样带上，删完才能回到原来那页）。
    hidden: String,
    at: String,
    method: String,
    path: String,
    status: i32,
    status_class: &'static str,
    duration: String,
    account: String,
    code: String,
    request: String,
}

impl LogRow {
    fn from_row(row: &CallRecordRow, filters: &Filters) -> Self {
        let status_class = if row.status >= 500 {
            "status status--bad"
        } else if row.status >= 400 {
            "status status--warn"
        } else {
            "status status--ok"
        };
        Self {
            id: row.id,
            back: filters.back_query(),
            hidden: filters.hidden_fields(),
            at: row.created_at.format("%m-%d %H:%M:%S").to_string(),
            method: row.method.clone(),
            path: row.path.clone(),
            status: row.status,
            status_class,
            duration: format!("{} ms", row.duration_ms),
            account: row
                .account_id
                .map(|id| id.to_string().chars().take(8).collect::<String>())
                .unwrap_or_default(),
            code: row.error_code.clone().unwrap_or_default(),
            request: row
                .request_id
                .map(|id| id.to_string().chars().take(8).collect::<String>())
                .unwrap_or_default(),
        }
    }
}

impl fmt::Display for LogRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tr><td class=\"num\">{}</td>", escape(&self.at))?;
        write!(f, "<td>{}</td>", escape(&self.method))?;
        write!(
            f,
            "<td class=\"path\" title=\"{}\">{}</td>",
            escape(&self.path),
            truncate(&self.path, 48)
        )?;
        write!(f, "<td><span class=\"{}\">{}</span></td>", self.status_class, self.status)?;
        write!(f, "<td class=\"num\">{}</td>", escape(&self.duration))?;
        write!(f, "<td class=\"muted\">{}</td>", escape(&self.account))?;
        write!(f, "<td class=\"muted\">{}</td>", escape(&self.code))?;
        write!(f, "<td class=\"muted\">{}</td>", escape(&self.request))?;
        // 详情带筛选条件回跳；删除就地提交，删完回到同一筛选下
        write!(
            f,
            "<td class=\"actions\"><a href=\"/logs/{}{}\">详情</a>",
            self.id, self.back
        )?;
        write!(
            f,
            "<form method=\"post\" action=\"/logs/{}/delete\">{}<button type=\"submit\" class=\"link link--danger\">删除</button></form></td></tr>",
            self.id,
            self.hidden
        )
    }
}



/// 日志页。
#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    nav: Vec<nav::NavItem>,
    cards: Vec<StatCard>,
    rows: Vec<LogRow>,
    error: String,
    hours: i64,
    status: String,
    path: String,
    code: String,
    pager: super::pagination::PageWindow,
    window_label: String,
    /// 上一次操作删掉了几条（>0 时页面顶部给一句回执）。
    deleted: i64,
}

/// `GET /logs`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let repo = CallRecordRepo::new(state.db.clone());
    let hours = filters.hours();
    let (cards, mut error) = match repo.summary(hours).await {
        Ok(summary) => {
            let success = if summary.total > 0 {
                format!("{:.1}%", summary.ok as f64 / summary.total as f64 * 100.0)
            } else {
                "-".to_string()
            };
            let cards = vec![
                StatCard::new("请求数", summary.total.to_string())
                    .sub(format!("近 {hours} 小时")),
                StatCard::new("成功率", success)
                    .sub(format!("成功 {} · 失败 {}", summary.ok, summary.failed)),
                StatCard::new(
                    "P50 延迟",
                    summary
                        .p50_ms
                        .map(|value| format!("{} ms", value.round() as i64))
                        .unwrap_or_else(|| "-".to_string()),
                ),
                StatCard::new(
                    "P95 延迟",
                    summary
                        .p95_ms
                        .map(|value| format!("{} ms", value.round() as i64))
                        .unwrap_or_else(|| "-".to_string()),
                ),
                StatCard::new(
                    "错误码 Top",
                    summary
                        .top_errors
                        .first()
                        .map(|(code, hits)| format!("{code}（{hits}）"))
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ];
            (cards, String::new())
        }
        Err(error) => (Vec::new(), format!("统计读取失败（{error}）")),
    };

    let size = super::pagination::page_size(&state.settings);
    let page = filters.page.max(1);
    let query = CallRecordQuery {
        offset: (page - 1) * size,
        limit: size,
        ..filters.to_query()
    };
    let total = match repo.count(&filters.to_query()).await {
        Ok(total) => total,
        Err(failure) => {
            if error.is_empty() {
                error = format!("总数读取失败（{failure}）");
            }
            0
        }
    };
    let rows = match repo.list(&query).await {
        Ok(rows) => rows
            .iter()
            .map(|row| LogRow::from_row(row, &filters))
            .collect::<Vec<_>>(),
        Err(failure) => {
            error = format!("明细读取失败（{failure}）");
            Vec::new()
        }
    };
    render(LogsPage {
        nav: nav::items("/logs"),
        cards,
        rows,
        error,
        hours,
        status: filters.status.clone(),
        path: filters.path.clone(),
        code: filters.code.clone(),
        pager: super::pagination::window(total, page, size, &filters.page_prefix()),
        window_label: format!("近 {hours} 小时"),
        deleted: filters.deleted,
    })
}

/// 详情页的一行字段。
pub struct DetailField {
    name: &'static str,
    value: String,
    /// 路径、UA 这类长文本用等宽字体方便核对。
    mono: bool,
}

impl DetailField {
    fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
            mono: false,
        }
    }

    fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    /// 空值显示成占位符，不留空白格。
    fn or_dash(mut self) -> Self {
        if self.value.trim().is_empty() {
            self.value = "-".to_string();
        }
        self
    }
}

/// 日志详情页。
#[derive(Template)]
#[template(path = "log_detail.html")]
struct LogDetailPage {
    nav: Vec<nav::NavItem>,
    title: String,
    fields: Vec<DetailField>,
    /// 删除本条的接口路径。
    delete_action: String,
    /// 返回列表的链接（带原筛选）。
    back: String,
    /// 删除表单的隐藏字段。
    hidden: String,
}

/// `GET /logs/{id}`
pub async fn detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Query(filters): Query<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let repo = CallRecordRepo::new(state.db.clone());
    let row = match repo.get(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::NOT_FOUND, "没有这条调用记录").into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取日志详情失败（{error}）"),
            )
                .into_response()
        }
    };

    let fields = vec![
        DetailField::new("时间", row.created_at.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string()),
        DetailField::new("方法", row.method.clone()),
        DetailField::new("路径", row.path.clone()).mono(),
        DetailField::new("状态", row.status.to_string()),
        DetailField::new("耗时", format!("{} ms", row.duration_ms)),
        DetailField::new(
            "账号",
            row.account_id.map(|id| id.to_string()).unwrap_or_default(),
        )
        .or_dash(),
        DetailField::new(
            "请求 ID",
            row.request_id.map(|id| id.to_string()).unwrap_or_default(),
        )
        .or_dash(),
        DetailField::new("调用方 IP", row.client_ip.clone().unwrap_or_default())
            .mono()
            .or_dash(),
        DetailField::new("API key", row.api_key_prefix.clone().unwrap_or_default())
            .mono()
            .or_dash(),
        DetailField::new("User-Agent", row.user_agent.clone().unwrap_or_default())
            .mono()
            .or_dash(),
        DetailField::new("错误码", row.error_code.clone().unwrap_or_default())
            .mono()
            .or_dash(),
        DetailField::new("失败原因", row.error_message.clone().unwrap_or_default())
            .mono()
            .or_dash(),
    ];

    render(LogDetailPage {
        nav: nav::items("/logs"),
        title: format!("{} {}（#{id}）", row.method, truncate(&row.path, 60)),
        fields,
        delete_action: format!("/logs/{id}/delete"),
        back: format!("/logs{}", filters.back_query()),
        hidden: filters.hidden_fields(),
    })
}

/// `POST /logs/{id}/delete` —— 删掉一条，回到原筛选的列表。
pub async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    axum::Form(filters): axum::Form<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let repo = CallRecordRepo::new(state.db.clone());
    let removed = match repo.delete(id).await {
        Ok(removed) => removed,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("删除日志失败（{error}）"),
            )
                .into_response()
        }
    };
    let deleted = if removed { 1 } else { 0 };
    Redirect::to(&format!("/logs{}&deleted={deleted}", filters.back_query())).into_response()
}

/// `POST /logs/delete` —— 把当前筛选下的记录整批删掉。
pub async fn remove_matching(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Form(filters): axum::Form<Filters>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let repo = CallRecordRepo::new(state.db.clone());
    let removed = match repo.delete_matching(&filters.to_query()).await {
        Ok(removed) => removed,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("批量删除日志失败（{error}）"),
            )
                .into_response()
        }
    };
    Redirect::to(&format!("/logs{}&deleted={removed}", filters.back_query()))
        .into_response()
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
    fn filters_translate_to_repo_query() {
        let filters = Filters {
            hours: 5000,
            status: "fail".to_string(),
            path: "/v1/images".to_string(),
            code: "upstream_error".to_string(),
            page: 2,
            deleted: 0,
        };
        let query = filters.to_query();
        assert_eq!(filters.hours(), 720, "小时数应被钳制");
        assert_eq!(query.status_range, Some((400, 599)));
        assert_eq!(query.path_prefix.as_deref(), Some("/v1/images"));
        assert_eq!(query.error_code.as_deref(), Some("upstream_error"));
        assert_eq!(query.offset, 0, "偏移由渲染时按页码注入");
        assert_eq!(
            filters.page_prefix(),
            "?hours=720&status=fail&path=%2Fv1%2Fimages&code=upstream_error&page="
        );    }

    #[test]
    fn delete_form_carries_current_filters() {
        let filters = Filters {
            hours: 6,
            status: "fail".to_string(),
            path: "/v1/images".to_string(),
            code: "upstream_error".to_string(),
            page: 3,
            deleted: 0,
        };
        // 删除后要回到同一筛选，页码不能带（回到第一页才看得到结果）
        assert_eq!(
            filters.back_query(),
            "?hours=6&status=fail&path=%2Fv1%2Fimages&code=upstream_error"
        );
        let hidden = filters.hidden_fields();
        assert!(hidden.contains("name=\"hours\" value=\"6\""), "{hidden}");
        assert!(hidden.contains("name=\"code\" value=\"upstream_error\""), "{hidden}");
        assert!(!hidden.contains("page"), "筛选表单不该带页码");
    }

    #[test]
    fn ok_status_maps_to_2xx() {
        let filters = Filters {
            status: "ok".to_string(),
            ..Default::default()
        };
        assert_eq!(filters.to_query().status_range, Some((200, 299)));
    }

    #[test]
    fn row_marks_status_and_escapes_path() {
        let filters = Filters::default();
        let row = LogRow {
            id: 42,
            back: filters.back_query(),
            hidden: filters.hidden_fields(),
            at: "01-01 00:00:00".to_string(),
            method: "POST".to_string(),
            path: "/v1/a<script>".to_string(),
            status: 502,
            status_class: "status status--bad",
            duration: "120 ms".to_string(),
            account: "abcd1234".to_string(),
            code: "upstream_error".to_string(),
            request: "deadbeef".to_string(),
        };
        let html = row.to_string();
        assert!(html.contains("status--bad"), "{html}");
        assert!(!html.contains("<script>"), "路径应被转义：{html}");
        assert!(html.contains("120 ms"), "{html}");
        // 每行都要能点进详情、也能就地删
        assert!(html.contains("href=\"/logs/42"), "{html}");
        assert!(html.contains("action=\"/logs/42/delete\""), "{html}");
    }
}

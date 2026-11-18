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
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            hours: 24,
            status: String::new(),
            path: String::new(),
            code: String::new(),
            page: 1,
        }
    }
}

impl Filters {
    fn hours(&self) -> i64 {
        // 没带参数时 serde 给的是 0，默认窗口按 24 小时算。
        let raw = if self.hours <= 0 { 24 } else { self.hours };
        raw.clamp(1, 24 * 30)
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
    fn from_row(row: &CallRecordRow) -> Self {
        let status_class = if row.status >= 500 {
            "status status--bad"
        } else if row.status >= 400 {
            "status status--warn"
        } else {
            "status status--ok"
        };
        Self {
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
        write!(f, "<td class=\"muted\">{}</td></tr>", escape(&self.request))
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
        Ok(rows) => rows.iter().map(LogRow::from_row).collect::<Vec<_>>(),
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
    })
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
    fn ok_status_maps_to_2xx() {
        let filters = Filters {
            status: "ok".to_string(),
            ..Default::default()
        };
        assert_eq!(filters.to_query().status_range, Some((200, 299)));
    }

    #[test]
    fn row_marks_status_and_escapes_path() {
        let row = LogRow {
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
    }
}

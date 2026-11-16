//! 密钥页：列表、新建、吊销与删除。
//!
//! 明文只在新建后的那一次响应里出现，之后库里只有 sha256；
//! 吊销保留记录，删除才是物理清理。

use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_storage::repo::ApiKeyRow;
use serde::Deserialize;
use uuid::Uuid;

use super::has_session;
use super::nav;
use crate::state::AppState;

/// 表格里的一行。
pub struct KeyRow {
    id: Uuid,
    name: String,
    prefix: String,
    created: String,
    last_used: String,
    revoked: bool,
}

impl KeyRow {
    fn from_row(row: &ApiKeyRow) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            prefix: row.prefix.clone(),
            created: row.created_at.format("%Y-%m-%d %H:%M").to_string(),
            last_used: row
                .last_used_at
                .map(|stamp| stamp.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "从未使用".to_string()),
            revoked: row.revoked_at.is_some(),
        }
    }
}

impl fmt::Display for KeyRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<tr id=\"key-{}\"><td class=\"pick\"><input type=\"checkbox\" name=\"ids\" value=\"{}\"></td>",
            self.id,
            self.id
        )?;
        write!(f, "<td>{}</td>", escape(&self.name))?;
        write!(
            f,
            "<td class=\"path\">{}</td>",
            escape(&self.prefix)
        )?;
        write!(f, "<td class=\"num\">{}</td>", escape(&self.created))?;
        write!(f, "<td class=\"muted\">{}</td>", escape(&self.last_used))?;
        let state = if self.revoked {
            "<span class=\"badge badge--disabled\">已吊销</span>"
        } else {
            "<span class=\"badge badge--normal\">生效中</span>"
        };
        write!(f, "<td>{state}</td>")?;
        write!(f, "<td class=\"actions\">")?;
        write!(
            f,
            "<a class=\"btn btn--ghost\" href=\"/keys/{}/edit\">改名</a>",
            self.id
        )?;
        if !self.revoked {
            write!(
                f,
                "<button class=\"btn btn--ghost\" hx-post=\"/keys/{}/revoke\" hx-disabled-elt=\"this\" hx-confirm=\"吊销后该 key 立即失效，确定？\">吊销</button>",
                self.id
            )?;
        }
        write!(
            f,
            "<button class=\"btn btn--danger\" hx-post=\"/keys/{}/delete\" hx-disabled-elt=\"this\" hx-confirm=\"删除不可恢复，确定删掉这条记录？\">删除</button>",
            self.id
        )?;
        write!(f, "</td></tr>")
    }
}

/// 每页条数。
const PAGE_SIZE: usize = 30;

/// 分页参数。
#[derive(Debug, Default, Deserialize)]
pub struct Pager {
    /// 偏移。
    #[serde(default)]
    pub offset: usize,
}

/// 密钥页。
#[derive(Template)]
#[template(path = "keys.html")]
struct KeysPage {
    nav: Vec<nav::NavItem>,
    rows: Vec<KeyRow>,
    plaintext: String,
    error: String,
    active_count: String,
    page_label: String,
    prev_href: String,
    next_href: String,
    has_prev: bool,
    has_next: bool,
    total: String,
}

/// 新建表单。
#[derive(Debug, Deserialize)]
pub struct CreateForm {
    /// 名称。
    #[serde(default)]
    pub name: String,
}

/// `GET /keys`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(pager): axum::extract::Query<Pager>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(page(&state, String::new(), String::new(), pager.offset).await)
}

async fn page(state: &AppState, plaintext: String, error: String, offset: usize) -> KeysPage {
    let (rows, list_error) = match state.api_keys.list().await {
        Ok(rows) => (rows.iter().map(KeyRow::from_row).collect(), error),
        Err(failure) => {
            let message = format!("读取失败（{failure}）");
            let combined = if error.is_empty() {
                message
            } else {
                format!("{error}；{message}")
            };
            (Vec::new(), combined)
        }
    };
    let active_count = match state.api_keys.active_count().await {
        Ok(count) => count.to_string(),
        Err(failure) => format!("读取失败（{failure}）"),
    };
    let total = rows.len();
    let offset = offset.min(total.saturating_sub(1));
    let page_rows: Vec<KeyRow> = rows.into_iter().skip(offset).take(PAGE_SIZE).collect();
    KeysPage {
        nav: nav::items("/keys"),
        rows: page_rows,
        plaintext,
        error: list_error,
        active_count,
        page_label: if total == 0 {
            String::new()
        } else {
            format!("第 {}–{} 条 / 共 {} 条", offset + 1, (offset + PAGE_SIZE).min(total), total)
        },
        prev_href: format!("/keys?offset={}", offset.saturating_sub(PAGE_SIZE)),
        next_href: format!("/keys?offset={}", offset + PAGE_SIZE),
        has_prev: offset > 0,
        has_next: offset + PAGE_SIZE < total,
        total: total.to_string(),
    }
}

/// `POST /keys/create`
pub async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let name = form.name.trim();
    let name = if name.is_empty() { "未命名" } else { name };
    match state.api_keys.create(name).await {
        Ok(created) => render(page(&state, created.plaintext, String::new(), 0).await),
        Err(failure) => {
            render(page(&state, String::new(), format!("创建失败：{failure}"), 0).await)
        }
    }
}

/// `POST /keys/{id}/revoke`
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match state.api_keys.revoke(id).await {
        Ok(true) => refresh(),
        Ok(false) => super::toast::oob_response("该 key 已是吊销状态或不存在", "error"),
        Err(failure) => {
            super::toast::oob_response(&format!("吊销失败：{failure}"), "error")
        }
    }
}

/// `POST /keys/{id}/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    match state.api_keys.delete(id).await {
        Ok(true) => refresh(),
        Ok(false) => super::toast::oob_response("记录不存在", "error"),
        Err(failure) => super::toast::oob_response(&format!("删除失败：{failure}"), "error"),
    }
}

/// 改名页。
#[derive(Template)]
#[template(path = "keys_edit.html")]
struct RenamePage {
    nav: Vec<nav::NavItem>,
    id: String,
    name: String,
    prefix: String,
    flash: String,
}

/// 改名表单。
#[derive(Debug, Deserialize)]
pub struct RenameForm {
    /// 新名称。
    #[serde(default)]
    pub name: String,
}

/// `GET /keys/{id}/edit`
pub async fn edit_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<super::accounts::FlashQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let rows = match state.api_keys.list().await {
        Ok(rows) => rows,
        Err(failure) => {
            return super::toast::oob_response(&format!("读取失败：{failure}"), "error");
        }
    };
    let Some(row) = rows.into_iter().find(|row| row.id == id) else {
        return Redirect::to("/keys").into_response();
    };
    render(RenamePage {
        nav: nav::items("/keys"),
        id: row.id.to_string(),
        name: row.name,
        prefix: row.prefix,
        flash: query.flash,
    })
}

/// `POST /keys/{id}/edit`
pub async fn edit_submit(
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
        return Redirect::to(&format!(
            "/keys/{id}/edit?flash={}",
            super::accounts::encode("名称不能为空")
        ))
        .into_response();
    }
    match state.api_keys.rename(id, name).await {
        Ok(true) => Redirect::to("/keys").into_response(),
        Ok(false) => super::toast::oob_response("记录不存在", "error"),
        Err(failure) => super::toast::oob_response(&format!("改名失败：{failure}"), "error"),
    }
}

/// 批量动作表单。
#[derive(Debug, Deserialize)]
pub struct BatchForm {
    /// 选中的 key。
    #[serde(default)]
    pub ids: Vec<Uuid>,
    /// revoke 或 delete。
    #[serde(default)]
    pub action: String,
}

/// `POST /keys/batch`
pub async fn batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let form = batch_form(&body);
    if form.ids.is_empty() {
        return super::toast::oob_response("先勾选要操作的密钥", "error");
    }
    let mut done = 0;
    let mut failed = 0;
    for id in &form.ids {
        let result = match form.action.as_str() {
            "revoke" => state.api_keys.revoke(*id).await,
            "delete" => state.api_keys.delete(*id).await,
            other => {
                return super::toast::oob_response(&format!("未知动作 {other}"), "error");
            }
        };
        match result {
            Ok(true) => done += 1,
            Ok(false) | Err(_) => failed += 1,
        }
    }
    if failed == 0 {
        return refresh();
    }
    super::toast::oob_response(&format!("完成 {done} 条，失败 {failed} 条"), "error")
}

/// 从原始表单里取出动作与 id 列表。
fn batch_form(body: &str) -> BatchForm {
    let mut form = BatchForm {
        ids: Vec::new(),
        action: String::new(),
    };
    for (key, value) in super::parse_form_pairs(body) {
        match key.as_str() {
            "ids" => {
                if let Ok(id) = value.parse::<Uuid>() {
                    form.ids.push(id);
                }
            }
            "action" => form.action = value,
            _ => {}
        }
    }
    form
}

/// 动作成功后让 htmx 重载当前页：分页与列表天然一致。
fn refresh() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert("hx-refresh", "true".parse().expect("常量"));
    response
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(revoked: bool) -> KeyRow {
        KeyRow {
            id: Uuid::nil(),
            name: "本机脚本".to_string(),
            prefix: "sk-g2a-12345678".to_string(),
            created: "2026-01-01 00:00".to_string(),
            last_used: "从未使用".to_string(),
            revoked,
        }
    }

    #[test]
    fn active_row_offers_revoke_and_delete() {
        let html = row(false).to_string();
        assert!(html.contains("生效中"), "{html}");
        assert!(html.contains("吊销"), "{html}");
        assert!(html.contains("删除"), "{html}");
        assert!(html.contains("sk-g2a-"), "{html}");
        assert!(html.contains("name=\"ids\""), "行内应有勾选框：{html}");
        assert!(html.contains("/edit"), "行内应有改名入口：{html}");
    }

    #[test]
    fn revoked_row_hides_revoke_button() {
        let html = row(true).to_string();
        assert!(html.contains("已吊销"), "{html}");
        assert!(!html.contains(">吊销<"), "{html}");
        assert!(html.contains("删除"), "{html}");
    }
}

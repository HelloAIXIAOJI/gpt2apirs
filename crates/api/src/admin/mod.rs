//! 管理后台页面：登录页与概览页。
//!
//! 页面用 Askama 在服务端渲染，浏览器只跑 htmx 做局部替换；
//! 鉴权看会话 cookie，数据接口仍走 `/api/*` 的中间件，这里不重复实现权限。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response, Redirect};
use chrono::Utc;
use serde::Deserialize;

use crate::auth::is_admin_token;
use crate::session;
use crate::state::AppState;

/// 提供给浏览器的 htmx 脚本。
pub const HTMX_JS: &str = include_str!("../../assets/htmx.min.js");

/// 后台样式表。
pub const ADMIN_CSS: &str = include_str!("../../assets/admin.css");

/// 登录页。
#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: String,
}

/// 概览页。
#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    account_count: String,
}

/// 登录表单。
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    /// 登录口令。
    pub key: String,
}

fn has_session(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(cookie) = headers.get(header::COOKIE).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(token) = session::cookie_value(Some(cookie), session::COOKIE_NAME) else {
        return false;
    };
    session::verify(state.session_secret.as_ref(), &token, Utc::now()).is_some()
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("hx-request")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// 跳转：htmx 请求用响应头导航，普通请求用 303。
fn navigate(headers: &HeaderMap, location: &str) -> Response {
    if is_htmx(headers) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        if let Ok(value) = location.parse() {
            response.headers_mut().insert("hx-redirect", value);
        }
        return response;
    }
    Redirect::to(location).into_response()
}

/// `GET /`：已登录进概览，否则是登录页。
pub async fn root(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if has_session(&state, &headers) {
        return Redirect::to("/overview").into_response();
    }
    render_login(String::new())
}

/// `GET /overview`：账号数量预览。
pub async fn overview(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let account_count = match state.pool.stats().await {
        Ok(stats) => stats.total.to_string(),
        Err(error) => format!("读取失败（{error}）"),
    };
    render(OverviewPage { account_count })
}

/// `POST /login`：校验口令，成功发会话 cookie 并跳到概览。
pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !is_admin_token(&state.config.admin_key, form.key.trim()) {
        return render_login("登录口令不正确".to_string());
    }
    let (token, _) = session::issue(state.session_secret.as_ref(), Utc::now());
    let cookie = session::cookie_header(&token, is_secure(&headers));
    let mut response = navigate(&headers, "/overview");
    match cookie.parse() {
        Ok(value) => {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        Err(_) => return render_login("会话 cookie 生成失败".to_string()),
    }
    response
}

/// `POST /logout`：清 cookie 回登录页。
pub async fn logout(headers: HeaderMap) -> Response {
    let mut response = navigate(&headers, "/");
    let cleared = format!(
        "{}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0",
        session::COOKIE_NAME
    );
    if let Ok(value) = cleared.parse() {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// `GET /admin/assets/htmx.min.js`
pub async fn htmx_asset() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], HTMX_JS).into_response()
}

/// `GET /admin/assets/admin.css`
pub async fn css_asset() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], ADMIN_CSS).into_response()
}

fn render_login(error: String) -> Response {
    render(LoginPage { error })
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
    fn login_page_links_assets_and_has_key_input() {
        let html = LoginPage { error: String::new() }.render().expect("渲染登录页");
        assert!(html.contains("/admin/assets/htmx.min.js"), "版式应引用 htmx：{html}");
        assert!(html.contains("/admin/assets/admin.css"), "版式应引用样式表：{html}");
        assert!(html.contains("name=\"key\""), "登录页要有口令输入框：{html}");
    }

    #[test]
    fn overview_renders_account_count() {
        let html = OverviewPage {
            account_count: "7".to_string(),
        }
        .render()
        .expect("渲染概览页");
        assert!(html.contains("账号数量"), "应显示指标名：{html}");
        assert!(html.contains(">7<"), "应显示数量 7：{html}");
    }

    #[test]
    fn css_asset_is_embedded() {
        assert!(ADMIN_CSS.contains("--accent"), "样式表应已打进二进制");
        assert!(ADMIN_CSS.contains("#login-card"), "登录卡片样式缺失");
    }

    #[test]
    fn htmx_asset_is_embedded() {
        assert!(HTMX_JS.len() > 30_000, "htmx 脚本应已打进二进制，实际 {} 字节", HTMX_JS.len());
        assert!(HTMX_JS.contains("htmx"), "内容不像 htmx");
    }
}

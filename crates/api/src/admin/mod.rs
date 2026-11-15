//! 管理后台页面：登录页与概览页。
//!
//! 页面用 Askama 在服务端渲染，浏览器只跑 htmx 做局部替换；
//! 鉴权看会话 cookie，数据接口仍走 `/api/*` 的中间件，这里不重复实现权限。

use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::Utc;
use gpt2apirs_storage::repo::ImageAssetRepo;
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

/// 账号指标。
struct AccountBlock {
    error: String,
    total: String,
    normal: String,
    limited: String,
    abnormal: String,
    disabled: String,
    quota_total: String,
    success: String,
    fail: String,
}

/// 图片占用。
struct ImageBlock {
    error: String,
    count: String,
    bytes: String,
}

/// 出口状态。
struct EgressBlock {
    state: String,
    nodes: String,
    selected: String,
    skipped: String,
}

/// 系统信息。
struct SystemBlock {
    uptime: String,
    version: String,
    database: String,
}

/// 概览页。
#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    accounts: AccountBlock,
    images: ImageBlock,
    active_keys: String,
    egress: EgressBlock,
    system: SystemBlock,
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

/// `GET /overview`：账号、图片、密钥、出口、系统的当前指标。
pub async fn overview(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(build_overview(&state).await)
}

async fn build_overview(state: &AppState) -> OverviewPage {
    let accounts = match state.pool.stats().await {
        Ok(stats) => AccountBlock {
            error: String::new(),
            total: stats.total.to_string(),
            normal: stats.normal.to_string(),
            limited: stats.limited.to_string(),
            abnormal: stats.abnormal.to_string(),
            disabled: stats.disabled.to_string(),
            quota_total: stats.total_quota.to_string(),
            success: stats.total_success.to_string(),
            fail: stats.total_fail.to_string(),
        },
        Err(error) => AccountBlock {
            error: format!("读取失败（{error}）"),
            ..empty_accounts()
        },
    };

    let images = match ImageAssetRepo::new(state.db.clone()).stats(None).await {
        Ok(stats) => ImageBlock {
            error: String::new(),
            count: stats.count.to_string(),
            bytes: format_bytes(stats.bytes.max(0) as u64),
        },
        Err(error) => ImageBlock {
            error: format!("读取失败（{error}）"),
            count: "-".to_string(),
            bytes: "-".to_string(),
        },
    };

    let active_keys = match state.api_keys.active_count().await {
        Ok(count) => count.to_string(),
        Err(error) => format!("读取失败（{error}）"),
    };

    let status = state.proxy_service.status();
    let egress = EgressBlock {
        state: if status.running { "运行中" } else { "未运行（直连）" }.to_string(),
        nodes: status.node_count.to_string(),
        selected: status.selected.unwrap_or_else(|| "未选择".to_string()),
        skipped: status.skipped.len().to_string(),
    };

    let database = match sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => "就绪".to_string(),
        Err(error) => format!("不可用（{error}）"),
    };
    let system = SystemBlock {
        uptime: format_uptime(state.uptime_secs().max(0) as u64),
        version: crate::system::APP_VERSION.to_string(),
        database,
    };

    OverviewPage {
        accounts,
        images,
        active_keys,
        egress,
        system,
    }
}

fn empty_accounts() -> AccountBlock {
    AccountBlock {
        error: String::new(),
        total: "-".to_string(),
        normal: "-".to_string(),
        limited: "-".to_string(),
        abnormal: "-".to_string(),
        disabled: "-".to_string(),
        quota_total: "-".to_string(),
        success: "-".to_string(),
        fail: "-".to_string(),
    }
}

/// 秒数转「N 天 N 小时 N 分」，只保留两级，避免一行太长。
fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else {
        format!("{minutes} 分")
    }
}

/// 字节数转 MB/GB，保留一位小数。
fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
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
    fn overview_renders_all_blocks() {
        let page = OverviewPage {
            accounts: AccountBlock {
                error: String::new(),
                total: "5".to_string(),
                normal: "3".to_string(),
                limited: "1".to_string(),
                abnormal: "1".to_string(),
                disabled: "0".to_string(),
                quota_total: "120".to_string(),
                success: "400".to_string(),
                fail: "9".to_string(),
            },
            images: ImageBlock {
                error: String::new(),
                count: "12".to_string(),
                bytes: "3.5 MB".to_string(),
            },
            active_keys: "2".to_string(),
            egress: EgressBlock {
                state: "运行中".to_string(),
                nodes: "44".to_string(),
                selected: "日本专线".to_string(),
                skipped: "3".to_string(),
            },
            system: SystemBlock {
                uptime: "2 天 3 小时".to_string(),
                version: "0.1.0".to_string(),
                database: "就绪".to_string(),
            },
        };
        let html = page.render().expect("渲染概览页");
        for expected in [
            "账号总数", "正常 3", "限流 1", "异常 1", "禁用 0", "120", "400 / 9",
            "12 张", "3.5 MB", "活跃密钥", "运行中", "44 个", "日本专线", "跳过 3 个",
            "2 天 3 小时", "0.1.0", "数据库", "就绪",
        ] {
            assert!(html.contains(expected), "缺少 {expected}：{html}");
        }
    }

    #[test]
    fn formats_uptime_and_bytes() {
        assert_eq!(format_uptime(30), "0 分");
        assert_eq!(format_uptime(3_700), "1 小时 1 分");
        assert_eq!(format_uptime(90_000), "1 天 1 小时");
        assert_eq!(format_bytes(512 * 1024), "512 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
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

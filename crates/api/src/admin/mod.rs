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

mod card;
mod nav;

use card::StatCard;
use nav::NavItem;

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

/// 卡片的展示分组。
struct StatGroup {
    title: &'static str,
    cards: Vec<StatCard>,
}

/// 概览页。
#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    groups: Vec<StatGroup>,
    nav: Vec<NavItem>,
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
    let mut groups = Vec::new();

    let accounts_cards = match state.pool.stats().await {
        Ok(stats) => vec![
            StatCard::new("账号总数", stats.total.to_string())
                .badge(format!("正常 {}", stats.normal))
                .badge(format!("限流 {}", stats.limited))
                .badge(format!("异常 {}", stats.abnormal))
                .badge(format!("禁用 {}", stats.disabled)),
            StatCard::new("正常账号配额总和", stats.total_quota.to_string()),
            StatCard::new("累计成功 / 失败", format!("{} / {}", stats.total_success, stats.total_fail)),
        ],
        Err(error) => vec![
            StatCard::failed("账号总数", format!("读取失败（{error}）")),
            StatCard::failed("正常账号配额总和", format!("读取失败（{error}）")),
            StatCard::failed("累计成功 / 失败", format!("读取失败（{error}）")),
        ],
    };
    groups.push(StatGroup {
        title: "账号",
        cards: accounts_cards,
    });

    let images_card = match ImageAssetRepo::new(state.db.clone()).stats(None).await {
        Ok(stats) => StatCard::new("图片存储", format!("{} 张", stats.count))
            .sub(format_bytes(stats.bytes.max(0) as u64)),
        Err(error) => StatCard::failed("图片存储", format!("读取失败（{error}）")),
    };
    let active_keys = match state.api_keys.active_count().await {
        Ok(count) => StatCard::new("活跃密钥", count.to_string()),
        Err(error) => StatCard::failed("活跃密钥", format!("读取失败（{error}）")),
    };
    groups.push(StatGroup {
        title: "资源",
        cards: vec![images_card, active_keys],
    });

    let status = state.proxy_service.status();
    let egress_card = StatCard::new(
        "代理出口",
        if status.running { "运行中" } else { "未运行（直连）" },
    )
    .sub(format!("节点 {} 个", status.node_count))
    .sub(format!(
        "当前 {}",
        status.selected.unwrap_or_else(|| "未选择".to_string())
    ))
    .sub(format!("跳过 {} 个", status.skipped.len()));
    groups.push(StatGroup {
        title: "出口",
        cards: vec![egress_card],
    });

    let database = match sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => StatCard::new("数据库", "就绪"),
        Err(error) => StatCard::failed("数据库", format!("不可用（{error}）")),
    };
    groups.push(StatGroup {
        title: "系统",
        cards: vec![
            StatCard::new("运行时长", format_uptime(state.uptime_secs().max(0) as u64)),
            StatCard::new("版本", crate::system::APP_VERSION),
            database,
        ],
    });

    OverviewPage {
        groups,
        nav: nav::items("/overview"),
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
    fn login_page_links_assets_and_has_no_sidebar() {
        let html = LoginPage { error: String::new() }.render().expect("渲染登录页");
        assert!(html.contains("/admin/assets/htmx.min.js"), "版式应引用 htmx：{html}");
        assert!(html.contains("/admin/assets/admin.css"), "版式应引用样式表：{html}");
        assert!(html.contains("name=\"key\""), "登录页要有口令输入框：{html}");
        assert!(!html.contains("id=\"sidebar\""), "登录页不该有侧边栏：{html}");
    }

    #[test]
    fn overview_renders_grouped_cards() {
        let page = OverviewPage {
            nav: nav::items("/overview"),
            groups: vec![
                StatGroup {
                    title: "账号",
                    cards: vec![StatCard::new("账号总数", "5")
                        .badge("正常 3")
                        .badge("禁用 0")],
                },
                StatGroup {
                    title: "系统",
                    cards: vec![StatCard::failed("数据库", "不可用（超时）")],
                },
            ],
        };
        let html = page.render().expect("渲染概览页");
        for expected in [
            "id=\"sidebar\"",
            "概览",
            "账号",
            "账号总数",
            ">5<",
            "正常 3",
            "系统",
            "数据库",
            "stat--danger",
            "is-active",
        ] {
            assert!(html.contains(expected), "缺少 {expected}：{html}");
        }
        assert!(html.contains("is-disabled"), "未建页面应显示为禁用项：{html}");
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

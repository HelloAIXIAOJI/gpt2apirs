//! 登录页与概览页的接口级测试：只验证页面行为，不连数据库。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const ADMIN_KEY: &str = "page-test-admin-key";
const UNREACHABLE_DB: &str = "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable";

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        ("DATABASE_URL", UNREACHABLE_DB),
        ("ADMIN_KEY", ADMIN_KEY),
    ]);
    let lookup = move |key: &str| map.get(key).map(|value| value.to_string()).filter(|v| !v.is_empty());
    let config = Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置"));
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy(UNREACHABLE_DB)
        .expect("lazy pool");
    Arc::new(AppState::new(
        config,
        Arc::new(RuntimeSettings::new(HashMap::new())),
        pool.clone(),
        Arc::new(
            gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default()).expect("client"),
        ),
        Arc::new(gpt2apirs_account::AccountPool::new(
            gpt2apirs_account::AccountRepo::new(pool.clone()),
            gpt2apirs_account::PoolConfig::default(),
        )),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new(),
        )),
    ))
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    cookie: Option<String>,
    body: String,
}

async fn send(method: &str, path: &str, body: Option<&str>, cookie: Option<&str>) -> Reply {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(value) = cookie {
        builder = builder.header("cookie", value);
    }
    let request = match body {
        Some(text) => builder
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(text.to_string()))
            .expect("请求"),
        None => builder.body(Body::empty()).expect("请求"),
    };
    let response = build_router(state()).oneshot(request).await.expect("响应");
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    Reply {
        status,
        location,
        cookie,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

#[tokio::test]
async fn root_shows_login_form() {
    let reply = send("GET", "/", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("name=\"key\""), "{}", reply.body);
    assert!(reply.body.contains("/admin/assets/htmx.min.js"), "{}", reply.body);
}

#[tokio::test]
async fn wrong_key_stays_on_login_page() {
    let reply = send("POST", "/login", Some("key=wrong"), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("登录口令不正确"), "{}", reply.body);
    assert!(reply.cookie.is_none(), "失败不该发 cookie");
}

#[tokio::test]
async fn correct_key_issues_cookie_and_opens_overview() {
    let reply = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "普通表单应 303 跳转");
    assert_eq!(reply.location.as_deref(), Some("/overview"));
    let cookie = reply.cookie.expect("应发会话 cookie");
    assert!(cookie.starts_with("g2a_admin="), "{cookie}");

    let overview = send("GET", "/overview", None, Some(&cookie)).await;
    assert_eq!(overview.status, StatusCode::OK);
    assert!(overview.body.contains("账号数量"), "{}", overview.body);
}

#[tokio::test]
async fn overview_requires_session() {
    let reply = send("GET", "/overview", None, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn css_asset_is_served() {
    let reply = send("GET", "/admin/assets/admin.css", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("#login-card"), "响应不像后台样式表");
}

#[tokio::test]
async fn htmx_asset_is_served() {
    let reply = send("GET", "/admin/assets/htmx.min.js", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("htmx"), "响应不像 htmx 脚本");
}

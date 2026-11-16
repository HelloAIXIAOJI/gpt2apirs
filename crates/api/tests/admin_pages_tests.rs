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
    send_state(state(), method, path, body, cookie).await
}

async fn send_state(
    state: Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<&str>,
    cookie: Option<&str>,
) -> Reply {
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
    let response = build_router(state).oneshot(request).await.expect("响应");
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
    assert!(overview.body.contains("账号总数"), "{}", overview.body);
    assert!(overview.body.contains("数据库"), "{}", overview.body);
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

#[tokio::test]
async fn accounts_page_requires_session() {
    let reply = send("GET", "/accounts", None, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn accounts_page_renders_for_logged_in_admin() {
    let login = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    let cookie = login.cookie.expect("登录应发 cookie");
    let reply = send("GET", "/accounts", None, Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("账号池"), "{}", reply.body);
    assert!(reply.body.contains("关键词"), "{}", reply.body);
}

#[tokio::test]
async fn import_with_empty_body_redirects_with_flash() {
    let login = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    let cookie = login.cookie.expect("cookie");
    let reply = send("POST", "/accounts/import", Some("text="), Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let location = reply.location.unwrap_or_default();
    assert!(location.starts_with("/accounts?flash="), "{location}");
}

#[tokio::test]
async fn edit_page_requires_session() {
    let reply = send(
        "GET",
        "/accounts/11111111-1111-1111-1111-111111111111/edit",
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn operation_fragment_requires_session() {
    let reply = send(
        "GET",
        "/accounts/operations/11111111-1111-1111-1111-111111111111",
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn oauth_start_returns_authorize_page() {
    let login = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    let cookie = login.cookie.expect("cookie");
    let reply = send(
        "POST",
        "/accounts/oauth/start",
        Some("email_hint=test%40example.com"),
        Some(&cookie),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("auth.openai.com"), "{}", reply.body);
    assert!(reply.body.contains("name=\"callback\""), "{}", reply.body);
}

#[tokio::test]
async fn settings_page_requires_session() {
    let reply = send("GET", "/settings", None, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn settings_page_lists_known_keys() {
    let login = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    let cookie = login.cookie.expect("cookie");
    let reply = send("GET", "/settings", None, Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("运行设置"), "{}", reply.body);
    assert!(reply.body.contains("image_poll_timeout_secs"), "{}", reply.body);
    assert!(reply.body.contains("proxy_subscription_url"), "{}", reply.body);
}

#[tokio::test]
async fn logs_page_requires_session() {
    let reply = send("GET", "/logs", None, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn logs_page_renders_filters() {
    let login = send("POST", "/login", Some(&format!("key={ADMIN_KEY}")), None).await;
    let cookie = login.cookie.expect("cookie");
    let reply = send("GET", "/logs", None, Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("路径前缀"), "{}", reply.body);
    assert!(reply.body.contains("错误码"), "{}", reply.body);
}

#[tokio::test]
async fn keys_page_full_cycle_against_memory_store() {
    let shared = state();
    let login = send_state(
        shared.clone(),
        "POST",
        "/login",
        Some(&format!("key={ADMIN_KEY}")),
        None,
    )
    .await;
    let cookie = login.cookie.expect("cookie");

    let empty = send_state(shared.clone(), "GET", "/keys", None, Some(&cookie)).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(empty.body.contains("还没有密钥"), "{}", empty.body);

    let created = send_state(
        shared.clone(),
        "POST",
        "/keys/create",
        Some("name=e2e"),
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK);
    assert!(created.body.contains("只显示这一次"), "{}", created.body);
    assert!(created.body.contains("sk-g2a-"), "新建后应显示明文：{}", created.body);
    assert!(created.body.contains("e2e"), "{}", created.body);

    let listed = send_state(shared.clone(), "GET", "/keys", None, Some(&cookie)).await;
    assert!(listed.body.contains("生效中"), "{}", listed.body);
    let id = listed
        .body
        .split("id=\"key-")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("能从页面里找到 key 的 id")
        .to_string();

    let revoked = send_state(
        shared.clone(),
        "POST",
        &format!("/keys/{id}/revoke"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "动作成功后让页面刷新");
    let after_revoke = send_state(shared.clone(), "GET", "/keys", None, Some(&cookie)).await;
    assert!(after_revoke.body.contains("已吊销"), "{}", after_revoke.body);

    let deleted = send_state(
        shared.clone(),
        "POST",
        &format!("/keys/{id}/delete"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let after = send_state(shared, "GET", "/keys", None, Some(&cookie)).await;
    assert!(after.body.contains("还没有密钥"), "{}", after.body);
}

#[tokio::test]
async fn keys_page_requires_session() {
    let reply = send("GET", "/keys", None, None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

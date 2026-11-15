//! 订阅出口管理接口的接口级测试（不需要真实节点，也不连数据库）。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const ADMIN_KEY: &str = "proxy-admin-key-123456";

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("ADMIN_KEY", ADMIN_KEY),
    ]);
    let lookup = move |key: &str| map.get(key).map(|v| v.to_string()).filter(|v| !v.is_empty());
    let config = Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置"));
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable")
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

async fn send(
    state: Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(value) = cookie {
        builder = builder.header("cookie", value);
    }
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("请求"),
        None => builder.body(Body::empty()).expect("请求"),
    };
    let response = build_router(state).oneshot(request).await.expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn login_cookie() -> String {
    let (_, _, cookie) = login().await;
    cookie
}

async fn login() -> (StatusCode, Value, String) {
    let state = state();
    let request = Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "key": ADMIN_KEY }).to_string()))
        .expect("请求");
    let response = build_router(state).oneshot(request).await.expect("响应");
    let status = response.status();
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), cookie)
}

#[tokio::test]
async fn proxy_routes_require_admin() {
    for path in ["/api/proxy/status", "/api/proxy/nodes"] {
        let (status, _) = send(state(), "GET", path, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} 应要求后台身份");
    }
}

#[tokio::test]
async fn status_reports_not_running_without_core() {
    let cookie = login_cookie().await;
    let (status, body) = send(state(), "GET", "/api/proxy/status", None, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["running"], false);
    assert_eq!(body["node_count"], 0);
}

#[tokio::test]
async fn probe_without_core_returns_empty_results() {
    let cookie = login_cookie().await;
    let (status, body) = send(
        state(),
        "POST",
        "/api/proxy/probe",
        Some(json!({})),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"].as_array().map(Vec::len), Some(0), "{body}");
}

#[tokio::test]
async fn select_without_core_fails() {
    let cookie = login_cookie().await;
    let (status, body) = send(
        state(),
        "POST",
        "/api/proxy/select",
        Some(json!({ "name": "n0" })),
        Some(&cookie),
    )
    .await;
    assert!(
        status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::SERVICE_UNAVAILABLE,
        "核没起时切换应失败：{status} {body}"
    );
}

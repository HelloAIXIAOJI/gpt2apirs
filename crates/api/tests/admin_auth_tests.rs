//! 后台登录与 API key 分离的验收测试。
//!
//! 核心不变量：登录口令调 /v1/* 必须失败；API key 进后台必须失败。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const ADMIN_KEY: &str = "admin-key-for-tests";

fn state() -> (Arc<AppState>, String) {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("AUTH_KEY", "legacy-auth-key-123"),
        ("ADMIN_KEY", ADMIN_KEY),
    ]);
    let lookup = move |key: &str| map.get(key).map(|v| v.to_string()).filter(|v| !v.is_empty());
    let config = Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置"));

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable")
        .expect("lazy pool");

    // 预置一把格式合法的 API key
    let seed = gpt2apirs_core::api_key::generate();
    let store = gpt2apirs_storage::repo::ApiKeyStore::Memory(
        gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(&seed.plaintext, "预置"),
    );

    let state = Arc::new(AppState::new(
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
        Arc::new(store),
    ));
    (state, seed.plaintext)
}

async fn send(
    state: Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<Value>,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(key) = bearer {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
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
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), set_cookie)
}

#[tokio::test]
async fn login_issues_httponly_cookie() {
    let (state, _) = state();
    let (status, body, cookie) = send(
        state,
        "POST",
        "/admin/login",
        Some(serde_json::json!({ "key": ADMIN_KEY })),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cookie = cookie.expect("应下发 Set-Cookie");
    assert!(cookie.contains("g2a_admin="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
}

#[tokio::test]
async fn login_rejects_wrong_key() {
    let (state, _) = state();
    let (status, _, _) = send(
        state,
        "POST",
        "/admin/login",
        Some(serde_json::json!({ "key": "wrong" })),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_cannot_enter_admin() {
    let (state, api_key) = state();
    let (status, _, _) = send(state, "GET", "/api/keys", None, Some(&api_key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "API key 不该能进后台");
}

#[tokio::test]
async fn admin_key_cannot_call_v1() {
    let (state, _) = state();
    let (status, _, _) = send(state, "GET", "/v1/models", None, Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "登录口令不该能调 /v1");
}

#[tokio::test]
async fn legacy_auth_key_is_not_a_login_key() {
    let (state, _) = state();
    let (status, _, _) = send(state, "GET", "/api/keys", None, Some("legacy-auth-key-123"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "旧 AUTH_KEY 不是登录口令");
}

#[tokio::test]
async fn api_key_can_call_v1() {
    let (state, api_key) = state();
    let (status, body, _) = send(state, "GET", "/v1/models", None, Some(&api_key), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"].is_array());
}

#[tokio::test]
async fn session_cookie_grants_admin_access() {
    let (state, _) = state();
    let (status, _, cookie) = send(
        state.clone(),
        "POST",
        "/admin/login",
        Some(serde_json::json!({ "key": ADMIN_KEY })),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cookie = cookie.expect("cookie");
    let token_part = cookie.split(';').next().expect("第一段");
    let (status, body, _) = send(
        state,
        "GET",
        "/api/keys",
        None,
        None,
        Some(token_part),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn created_key_works_and_revoked_key_stops_working() {
    let (state, _) = state();
    let (status, body, _) = send(
        state.clone(),
        "POST",
        "/api/keys",
        Some(serde_json::json!({ "name": "新 key" })),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plaintext = body["plaintext"].as_str().expect("应返回一次明文").to_string();
    let id = body["id"].as_str().expect("应有 id").to_string();
    assert!(plaintext.starts_with("sk-g2a-"));

    // 新 key 能用
    let (status, _, _) = send(state.clone(), "GET", "/v1/models", None, Some(&plaintext), None).await;
    assert_eq!(status, StatusCode::OK, "新建的 key 应能调用");

    // 列表里只有前缀，没有明文
    let (status, list, _) = send(state.clone(), "GET", "/api/keys", None, Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let serialized = list.to_string();
    assert!(!serialized.contains(&plaintext), "列表不该包含明文");

    // 吊销后立刻失效
    let (status, _, _) = send(
        state.clone(),
        "DELETE",
        &format!("/api/keys/{id}"),
        None,
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(state, "GET", "/v1/models", None, Some(&plaintext), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "吊销后的 key 必须失效");
}

#[tokio::test]
async fn x_api_key_header_works_like_bearer() {
    // Anthropic 客户端只发 x-api-key，不发 Authorization
    let (state, api_key) = state();
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header("x-api-key", &api_key)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn x_api_key_header_cannot_enter_admin() {
    let (state, api_key) = state();
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/keys")
                .header("x-api-key", &api_key)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "x-api-key 不该能进后台");
}

#[tokio::test]
async fn x_api_key_with_admin_key_still_fails_v1() {
    let (state, _) = state();
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header("x-api-key", ADMIN_KEY)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "登录口令不该因换个头就通过");
}

//! `/v1/search` 的参数校验测试（全部离线）。

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

const AUTH_KEY: &str = "test-admin-key-123456";

fn account_pool(db: &sqlx::PgPool) -> Arc<gpt2apirs_account::AccountPool> {
    Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(db.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ))
}

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("AUTH_KEY", AUTH_KEY),
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
        account_pool(&pool),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
        // 调用 /v1/* 用 API key：测试里预置一把与 AUTH_KEY 相同的 key
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(AUTH_KEY, "test"),
        )),
    ))
}

async fn post_search(body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("content-type", "application/json");
    if let Some(key) = auth {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder
        .body(Body::from(body.to_string()))
        .expect("请求");
    let response = build_router(state()).oneshot(request).await.expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn requires_auth() {
    let (status, _) = post_search(serde_json::json!({ "prompt": "x" }), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_empty_prompt() {
    let (status, body) = post_search(serde_json::json!({ "prompt": "   " }), Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn empty_pool_reports_unavailable() {
    let (status, body) = post_search(serde_json::json!({ "prompt": "今天北京天气" }), Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "no_available_account");
}

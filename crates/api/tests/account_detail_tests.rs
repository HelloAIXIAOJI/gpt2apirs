//! `GET /api/accounts/{id}` 的路由契约测试（离线）。
//!
//! 离线只验「请求确实走到仓储层、路径 id 被解析、鉴权照旧」：
//! DB 指向不可达地址，所以合法 UUID 的期望结果是 500 而不是 415/404。

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
use uuid::Uuid;

const ADMIN_KEY: &str = "admin-key-for-detail-tests";

fn state() -> Arc<AppState> {
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
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key("unused", "test"),
        )),
    ))
}

async fn get(path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state())
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 合法 UUID：请求必须走到仓储层（连不上库 → 500），而不是被路由或提取器挡掉。
#[tokio::test]
async fn valid_id_reaches_the_repository() {
    let id = Uuid::new_v4();
    let (status, body) = get(&format!("/api/accounts/{id}"), Some(ADMIN_KEY)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
}

/// 非法 UUID 由 axum 直接拒。
#[tokio::test]
async fn bad_id_is_rejected() {
    let (status, _) = get("/api/accounts/not-a-uuid", Some(ADMIN_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 鉴权照旧：没有登录口令进不来。
#[tokio::test]
async fn requires_admin() {
    let id = Uuid::new_v4();
    let (status, _) = get(&format!("/api/accounts/{id}"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// API key 进不了后台接口（两套凭证不互通的既有不变量）。
#[tokio::test]
async fn api_key_cannot_read_it() {
    let id = Uuid::new_v4();
    let (status, _) = get(&format!("/api/accounts/{id}"), Some("sk-g2a-not-an-admin")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

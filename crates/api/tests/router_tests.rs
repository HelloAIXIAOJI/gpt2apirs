//! 路由与鉴权的集成测试。
//!
//! 用 `connect_lazy` 建池 —— 不需要真实数据库也能测路由与鉴权。
//! 需要真实数据库的健康检查另有一个测试（有 `DATABASE_URL` 才跑）。

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

fn config() -> Arc<BootstrapConfig> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("AUTH_KEY", AUTH_KEY),
    ]);
    let lookup = move |key: &str| {
        map.get(key)
            .map(|v| v.to_string())
            .filter(|v| !v.is_empty())
    };
    Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置应可构造"))
}

fn state_without_db() -> Arc<AppState> {
    // connect_lazy：不真连；健康检查会失败  degraded，这是预期行为
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable")
        .expect("lazy pool 应可构造");
    Arc::new(AppState::new(
        config(),
        Arc::new(RuntimeSettings::new(HashMap::new())),
        pool.clone(),
        Arc::new(gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default()).expect("客户端")),
        account_pool(&pool),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
    ))
}

async fn send(
    app: axum::Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("构造请求"))
        .await
        .expect("请求应成功");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("读 body");
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn account_pool(db: &sqlx::PgPool) -> Arc<gpt2apirs_account::AccountPool> {
    Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(db.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ))
}

#[tokio::test]
async fn version_needs_no_auth() {
    let (status, body) = send(build_router(state_without_db()), "GET", "/version", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn health_is_always_200_with_details() {
    let (status, body) = send(build_router(state_without_db()), "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK, "健康端点应总是 200");
    // 没有可用数据库  degraded
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["healthy"], false);
    assert_eq!(body["checks"]["database"], "error");
    assert!(body["version"].is_string());
    assert!(body["uptime_secs"].is_number());
}

#[tokio::test]
async fn ready_returns_503_without_database() {
    let (status, body) = send(build_router(state_without_db()), "GET", "/health/ready", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "not_ready");
}

#[tokio::test]
async fn admin_route_requires_auth() {
    let (status, body) = send(build_router(state_without_db()), "GET", "/api/ping", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // 管理面格式：{"detail": {...}}
    assert!(body["detail"].is_object(), "应为管理面格式: {body}");
    assert_eq!(body["detail"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn admin_route_rejects_wrong_key() {
    let (status, _) = send(
        build_router(state_without_db()),
        "GET",
        "/api/ping",
        Some("wrong-key"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_route_accepts_correct_key() {
    let (status, body) = send(
        build_router(state_without_db()),
        "GET",
        "/api/ping",
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pong"], true);
}

#[tokio::test]
async fn unknown_path_returns_404_in_openai_format() {
    let (status, body) = send(build_router(state_without_db()), "GET", "/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_object(), "应为 OpenAI 格式: {body}");
    assert_eq!(body["error"]["code"], "bad_request");
}

/// 有真实数据库时，健康检查应报 ok。
#[tokio::test]
async fn health_reports_ok_with_real_database() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        manifest.join("../../.env"),
        manifest.join("../../target/debug/.env"),
    ] {
        if path.is_file() {
            let _ = dotenvy::from_path(&path);
            break;
        }
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("应能连接");
    let state = Arc::new(AppState::new(
        config(),
        Arc::new(RuntimeSettings::new(HashMap::new())),
        pool.clone(),
        Arc::new(gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default()).expect("客户端")),
        account_pool(&pool),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
    ));

    let (status, body) = send(build_router(state), "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok", "有数据库时应为 ok: {body}");
    assert_eq!(body["healthy"], true);
    assert_eq!(body["checks"]["database"], "ok");
}

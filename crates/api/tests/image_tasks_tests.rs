//! 异步生图任务的提交校验（离线）。
//!
//! 任务本身会在后台线程里去做真上游；这里只验证「参数能不能正确进到请求里」，
//! 所以断言到 200 + queued 为止。

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

const TEST_KEY: &str = "test-admin-key-123456";

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
        ("ADMIN_KEY", TEST_KEY),
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
        // 调用 /v1/* 用 API key：测试里预置一把与 TEST_KEY 相同的 key
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(TEST_KEY, "test"),
        )),
    ))
}

fn assert_openai_error(body: &Value, code: &str) {
    assert_eq!(body["error"]["code"], code, "{body}");
}

async fn post_task(body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/image-tasks/generations")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state())
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).expect("序列化")))
                .expect("request"),
        )
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn requires_auth() {
    let (status, _) = post_task(serde_json::json!({ "prompt": "x" }), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_gif_frame_parameters() {
    // 回归点：异步任务以前只认 output_format，frame_count / frame_duration_ms 会被静默丢掉。
    let (status, body) = post_task(
        serde_json::json!({
            "prompt": "一个转圈的猫",
            "model": "image",
            "n": 1,
            "output_format": "gif",
            "frame_count": 12,
            "frame_duration_ms": 80
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "queued", "{body}");
    assert!(body["id"].is_string(), "{body}");
    // 任务是异步的，唯一能证明「参数被读到」的就是任务视图回显。
    assert_eq!(body["output_format"], "gif", "{body}");
    assert_eq!(body["frame_count"], 12, "{body}");
    assert_eq!(body["frame_duration_ms"], 80, "{body}");
}

#[tokio::test]
async fn rejects_bad_output_format() {
    let (status, body) = post_task(
        serde_json::json!({ "prompt": "x", "output_format": "bmp" }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_openai_error(&body, "bad_request");
}

#[tokio::test]
async fn rejects_n_out_of_range() {
    let (status, body) = post_task(
        serde_json::json!({ "prompt": "x", "n": 9 }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

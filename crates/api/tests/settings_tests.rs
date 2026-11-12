//! `/api/settings` 的离线测试：读取形状 + 写入校验。
//!
//! 写入成功路径需要真实 PostgreSQL，见 `settings_live.rs`。

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

const AUTH_KEY: &str = "test-admin-key-123456";

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
        Arc::new(gpt2apirs_account::AccountPool::new(
            gpt2apirs_account::AccountRepo::new(pool.clone()),
            gpt2apirs_account::PoolConfig::default(),
        )),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
    ))
}

async fn send(method: &str, body: Option<Value>, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri("/api/settings")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).expect("序列化")),
        None => Body::empty(),
    };
    let response = build_router(state())
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn get_settings_requires_auth() {
    let (status, _) = send("GET", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_settings_lists_numeric_and_string_specs() {
    let (status, body) = send("GET", None, Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let numerics = body["numerics"].as_array().expect("numerics");
    assert_eq!(
        numerics.len(),
        gpt2apirs_core::settings::NUMERIC_SPECS.len(),
        "返回的项数应等于契约项数"
    );

    // 抽查一项：默认值 / 上下限 / 类型都在
    let concurrency = numerics
        .iter()
        .find(|item| item["key"] == "image_account_concurrency")
        .expect("应包含 image_account_concurrency");
    assert_eq!(concurrency["default"], 1.0);
    assert_eq!(concurrency["min"], 1.0);
    assert_eq!(concurrency["max"], 3.0);
    assert_eq!(concurrency["kind"], "integer");

    let strings = body["strings"].as_array().expect("strings");
    assert_eq!(
        strings.len(),
        gpt2apirs_core::settings::STRING_SPECS.len(),
        "返回的字符串项数应等于契约项数"
    );
    let keywords = strings
        .iter()
        .find(|item| item["key"] == "chat_blocked_keywords")
        .expect("应包含 chat_blocked_keywords");
    assert_eq!(keywords["value"], "", "默认空串");
    let policy = strings
        .iter()
        .find(|item| item["key"] == "rich_text_policy")
        .expect("应包含 rich_text_policy");
    assert_eq!(policy["value"], "guard+strip", "默认策略");
}

#[tokio::test]
async fn put_rejects_empty_values() {
    let (status, body) = send("PUT", Some(json!({"values": {}})), Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["detail"]["param"], "values", "{body}");
}

#[tokio::test]
async fn put_rejects_unknown_key() {
    let (status, body) = send(
        "PUT",
        Some(json!({"values": {"no_such_setting": 1}})),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["detail"]["code"], "unknown_setting_key", "{body}");
    assert!(
        body["detail"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("no_such_setting"),
        "{body}"
    );
}

#[tokio::test]
async fn put_rejects_non_numeric_value_for_numeric_key() {
    let (status, body) = send(
        "PUT",
        Some(json!({"values": {"image_account_concurrency": "很多"}})),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["detail"]["param"], "image_account_concurrency", "{body}");
}

#[tokio::test]
async fn put_rejects_object_value_for_string_key() {
    let (status, body) = send(
        "PUT",
        Some(json!({"values": {"chat_blocked_keywords": {"oops": true}}})),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["detail"]["param"], "chat_blocked_keywords", "{body}");
}

#[tokio::test]
async fn put_valid_passes_validation_then_tries_database() {
    // 校验通过后才会去写库；这里的库是故意连不上的，
    // 所以断言「不是 400」——说明请求体已被接受，走到了持久化阶段。
    let (status, body) = send(
        "PUT",
        Some(json!({"values": {"image_account_concurrency": 2, "chat_blocked_keywords": "a,b"}})),
        Some(AUTH_KEY),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "合法请求不应被校验拒绝：{body}"
    );
}

#[tokio::test]
async fn put_requires_auth() {
    let (status, _) = send(
        "PUT",
        Some(json!({"values": {"image_account_concurrency": 2}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

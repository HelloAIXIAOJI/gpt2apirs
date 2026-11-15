//! `/v1/models` 与 `/v1/models/{id}` 的离线测试。
//!
//! 两个端点必须**给出同一个答案**：列表里有的 id，单查必须命中；
//! 单查 404 的错误码必须是 `model_not_found`（OpenAI 客户端按它分支）。

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

const API_KEY: &str = "test-api-key-123456";

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("ADMIN_KEY", "test-admin-key-123456"),
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
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(API_KEY, "test"),
        )),
    ))
}

/// 发一个 GET，可带 `Authorization` 或 `x-api-key`。
async fn get(path: &str, bearer: Option<&str>, x_api_key: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(token) = x_api_key {
        builder = builder.header("x-api-key", token);
    }
    let response = build_router(state())
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn ids(body: &Value) -> Vec<String> {
    body["data"]
        .as_array()
        .expect("data 应为数组")
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn requires_api_key() {
    let (status, body) = get("/v1/models", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_api_key", "错误体应是 OpenAI 格式：{body}");
}

#[tokio::test]
async fn lists_both_text_and_image_models() {
    let (status, body) = get("/v1/models", Some(API_KEY), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "list");

    let ids = ids(&body);
    assert!(ids.contains(&"image".to_string()), "应列出图片模型：{ids:?}");
    assert!(ids.contains(&"text".to_string()), "应列出文本模型：{ids:?}");
    assert_eq!(ids.len(), 2, "只暴露 text 与 image 两个模型：{ids:?}");
}

#[tokio::test]
async fn every_entry_has_model_shape_and_kind() {
    let (_, body) = get("/v1/models", Some(API_KEY), None).await;
    for item in body["data"].as_array().expect("data") {
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "gpt2apirs");
        assert_eq!(item["root"], item["id"]);
        let kind = item["kind"].as_str().unwrap_or("");
        assert!(kind == "text" || kind == "image", "kind 取值非法：{item}");
    }
}

#[tokio::test]
async fn text_model_is_declared_as_text() {
    let (status, body) = get("/v1/models/text", Some(API_KEY), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "text");
    assert_eq!(body["object"], "model");
    assert_eq!(body["kind"], "text");
}


#[tokio::test]
async fn unknown_model_is_404_with_model_not_found() {
    let (status, body) = get("/v1/models/dall-e-3", Some(API_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "model_not_found", "{body}");
    assert_eq!(body["error"]["param"], Value::Null);
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("dall-e-3"),
        "错误信息应带上模型名：{body}"
    );
}

#[tokio::test]
async fn single_lookup_agrees_with_list() {
    let (_, list) = get("/v1/models", Some(API_KEY), None).await;
    for id in ids(&list) {
        let (status, body) = get(&format!("/v1/models/{id}"), Some(API_KEY), None).await;
        assert_eq!(status, StatusCode::OK, "{id} 在列表里却查不到：{body}");
        assert_eq!(body["id"], id.as_str());
    }
}

#[tokio::test]
async fn x_api_key_header_is_accepted() {
    let (status, body) = get("/v1/models", None, Some(API_KEY)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "list");
}

#[tokio::test]
async fn unknown_x_api_key_is_rejected() {
    let (status, body) = get("/v1/models", None, Some("sk-g2a-not-a-real-key")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_api_key", "{body}");
}

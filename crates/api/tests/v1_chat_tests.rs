//! `/v1/chat/completions` 的**输入校验与内容粗筛**测试。
//!
//! 全部离线：账号池为空，所以合法请求的终点是 `503 no_available_account` ——
//! 能走到取号，就证明请求解析、角色归一、内容提取都通了。

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

const TEST_KEY: &str = "test-admin-key-123456";

fn account_pool(db: &sqlx::PgPool) -> Arc<gpt2apirs_account::AccountPool> {
    Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(db.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ))
}

fn state_with(settings: HashMap<String, Value>) -> Arc<AppState> {
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
        Arc::new(RuntimeSettings::new(settings)),
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

fn state() -> Arc<AppState> {
    state_with(HashMap::new())
}

async fn post(body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    post_with(state(), body, auth).await
}

async fn post_with(
    state: Arc<AppState>,
    body: Value,
    auth: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state)
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

fn simple_body(prompt: &str) -> Value {
    json!({
        "model": "text",
        "messages": [{ "role": "user", "content": prompt }],
    })
}

#[tokio::test]
async fn requires_auth() {
    let (status, body) = post(simple_body("hi"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_api_key", "{body}");
}

#[tokio::test]
async fn rejects_empty_messages() {
    let (status, body) = post(json!({"messages": []}), Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "messages", "{body}");
}

#[tokio::test]
async fn rejects_unknown_role() {
    let body = json!({"messages": [{"role": "tool", "content": "x"}]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("tool"),
        "{response}"
    );
}

/// 图文对话：图片被接受，请求走到取号（账号池为空 → 503）。
#[tokio::test]
async fn accepts_image_content() {
    let body = json!({"messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "这是什么"},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
        ],
    }]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert_eq!(response["error"]["code"], "no_available_account");
}

/// 只有图片、没有文本也要放行（图片本身就是内容）。
#[tokio::test]
async fn accepts_image_only_message() {
    let body = json!({"messages": [{
        "role": "user",
        "content": [{"type": "image_url", "image_url": {"url": "https://x/y.png"}}],
    }]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
}

/// 图片 part 缺少地址要说清是哪个字段。
#[tokio::test]
async fn rejects_image_part_without_url() {
    let body = json!({"messages": [{
        "role": "user",
        "content": [{"type": "image_url", "image_url": {}}],
    }]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["param"], "messages", "{response}");
}

#[tokio::test]
async fn rejects_all_blank_content() {
    let body = json!({"messages": [{"role": "user", "content": "   "}]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["param"], "messages", "{response}");
}

#[tokio::test]
async fn accepts_array_content_with_text_parts() {
    let body = json!({"messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "第一段"},
            {"type": "text", "text": "第二段"},
        ],
    }]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    // 解析通过  走到取号  池空
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert_eq!(response["error"]["code"], "no_available_account");
}

#[tokio::test]
async fn valid_request_reaches_account_pool() {
    let (status, body) = post(simple_body("介绍一下你自己"), Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account", "{body}");
}

#[tokio::test]
async fn multi_turn_history_reaches_account_pool() {
    let body = json!({"messages": [
        {"role": "system", "content": "你是助手"},
        {"role": "user", "content": "第一问"},
        {"role": "assistant", "content": "第一答"},
        {"role": "user", "content": "第二问"},
    ]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
}

#[tokio::test]
async fn stream_flag_still_reaches_account_pool() {
    let mut body = simple_body("hi");
    body["stream"] = json!(true);
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
}

#[tokio::test]
async fn content_filter_blocks_configured_keyword() {
    let mut settings = HashMap::new();
    settings.insert(
        "chat_blocked_keywords".to_string(),
        json!("违禁词,另一个词"),
    );
    let body = json!({"messages": [{"role": "user", "content": "这句话里有违禁词"}]});
    let (status, response) = post_with(state_with(settings), body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["code"], "content_policy_violation", "{response}");
}

#[tokio::test]
async fn content_filter_is_case_insensitive() {
    let mut settings = HashMap::new();
    settings.insert("chat_blocked_keywords".to_string(), json!("Forbidden"));
    let body = json!({"messages": [{"role": "user", "content": "this is FORBIDDEN stuff"}]});
    let (status, response) = post_with(state_with(settings), body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["code"], "content_policy_violation");
}

#[tokio::test]
async fn content_filter_disabled_when_list_empty() {
    // 默认（未配置）不做任何拦截
    let body = json!({"messages": [{"role": "user", "content": "违禁词"}]});
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "未配置关键词时不应拦截：{response}"
    );
}

#[tokio::test]
async fn content_filter_ignores_assistant_messages() {
    // 只筛用户侧，历史里的 assistant 内容不重复触发
    let mut settings = HashMap::new();
    settings.insert("chat_blocked_keywords".to_string(), json!("违禁词"));
    let body = json!({"messages": [
        {"role": "user", "content": "正常问题"},
        {"role": "assistant", "content": "违禁词"},
        {"role": "user", "content": "继续"},
    ]});
    let (status, response) = post_with(state_with(settings), body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
}

// ---------------- modalities ----------------

/// 只要文本：照常放行（账号池为空，终点是 503）。
#[tokio::test]
async fn accepts_text_only_modalities() {
    let mut body = simple_body("hi");
    body["modalities"] = json!(["text"]);
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert_eq!(response["error"]["code"], "no_available_account");
}

/// 要图片模态：必须显式拒绝，不能默默只回文本。
#[tokio::test]
async fn rejects_image_modality() {
    let mut body = simple_body("画一只猫");
    body["modalities"] = json!(["text", "image"]);
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["param"], "modalities", "{response}");
    let message = response["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("images/generations"),
        "错误信息应指路生图端点：{message}"
    );
}

/// 只写 image 也一样拒。
#[tokio::test]
async fn rejects_image_only_modality() {
    let mut body = simple_body("画一只猫");
    body["modalities"] = json!(["image"]);
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["code"], "bad_request", "{response}");
}

/// 形态不对也要说清楚是哪个字段。
#[tokio::test]
async fn rejects_malformed_modalities() {
    let mut body = simple_body("hi");
    body["modalities"] = json!("text");
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["param"], "modalities", "{response}");
}

/// 不传或传 null 都不该被拦。
#[tokio::test]
async fn null_modalities_is_accepted() {
    let mut body = simple_body("hi");
    body["modalities"] = Value::Null;
    let (status, response) = post(body, Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
}

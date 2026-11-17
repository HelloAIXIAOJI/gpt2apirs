//! `/v1/responses` 与 `/v1/messages` 的离线测试。
//!
//! 账号池为空，所以合法请求的终点是 `503 no_available_account` ——
//! 能走到取号就证明请求解析、角色归一、内容提取全通。

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
        Arc::new(gpt2apirs_account::AccountPool::new(
            gpt2apirs_account::AccountRepo::new(pool.clone()),
            gpt2apirs_account::PoolConfig::default(),
        )),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
        // 调用 /v1/* 用 API key：测试里预置一把与 TEST_KEY 相同的 key
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(TEST_KEY, "test"),
        )),
    ))
}

async fn post(path: &str, body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
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

// ---------------- /v1/responses ----------------

#[tokio::test]
async fn responses_requires_auth() {
    let (status, _) = post("/v1/responses", json!({"input": "hi"}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn responses_accepts_plain_string_input() {
    let (status, body) = post("/v1/responses", json!({"input": "你好"}), Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account");
}

#[tokio::test]
async fn responses_accepts_messages_array_with_instructions() {
    let (status, body) = post(
        "/v1/responses",
        json!({
            "instructions": "你是简洁的助手",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "第一问"}]},
                {"role": "assistant", "content": "第一答"},
                {"role": "user", "content": "第二问"},
            ],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn responses_accepts_bare_content_parts() {
    // input 直接是内容片段数组（没有 role）
    let (status, body) = post(
        "/v1/responses",
        json!({"input": [{"type": "input_text", "text": "只有片段"}]}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn responses_rejects_empty_input() {
    let (status, body) = post("/v1/responses", json!({"input": "   "}), Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["param"], "input");
}

#[tokio::test]
async fn responses_rejects_tools() {
    let (status, body) = post(
        "/v1/responses",
        json!({
            "input": "hi",
            "tools": [{"type": "function", "name": "f"}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "tools_not_supported", "{body}");
}

#[tokio::test]
async fn responses_rejects_image_input() {
    let (status, body) = post(
        "/v1/responses",
        json!({"input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://x/y.png"}
        ]}]}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("images/edits"),
        "{body}"
    );
}

// ---------------- /v1/messages ----------------

/// 图文对话：Anthropic 的 base64 图片块被接受，请求走到取号（池空 → 503）。
#[tokio::test]
async fn messages_accepts_base64_image_block() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 128,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "这是什么"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png",
                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
                    }},
                ],
            }],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account");
}

/// url 形态的图片块同样接受。
#[tokio::test]
async fn messages_accepts_url_image_block() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 128,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}},
            ]}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

/// base64 不合法要报参数错误而不是崩。
#[tokio::test]
async fn messages_rejects_broken_base64_image() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 32,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "!!!"}},
            ]}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], "error", "{body}");
}

#[tokio::test]
async fn messages_requires_auth() {
    let (status, body) = post(
        "/v1/messages",
        json!({"max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Anthropic 客户端只解析 {"type":"error",...}，401 也必须是这个形状
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "authentication_error", "{body}");
}

#[tokio::test]
async fn messages_accepts_simple_request() {
    let (status, body) = post(
        "/v1/messages",
        json!({"max_tokens": 256, "messages": [{"role": "user", "content": "你好"}]}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account");
}

#[tokio::test]
async fn messages_accepts_system_and_block_content() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 256,
            "system": [{"type": "text", "text": "你是助手"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "第一问"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "第一答"}]},
                {"role": "user", "content": "第二问"},
            ],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn messages_rejects_reasoning_role() {
    // Anthropic 只有 user / assistant；system 放顶层
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 100,
            "messages": [{"role": "system", "content": "x"}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("system"),
        "错误信息应提示 system 放顶层：{body}"
    );
}

#[tokio::test]
async fn messages_uses_anthropic_error_shape() {
    let (status, body) = post("/v1/messages", json!({"max_tokens": 0, "messages": []}), Some(TEST_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // Anthropic 错误格式：{"type":"error","error":{...}}
    assert_eq!(body["type"], "error", "应为 Anthropic 错误格式：{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

#[tokio::test]
async fn messages_rejects_tools() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "f", "input_schema": {}}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // Anthropic 错误体只有 type + message，没有自定义 code 字段
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("不支持工具调用"),
        "{body}"
    );
}

#[tokio::test]
async fn messages_rejects_empty_messages() {
    let (status, body) = post(
        "/v1/messages",
        json!({"max_tokens": 100, "messages": []}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // Anthropic 错误格式没有 param 字段（规范只有 type + message）
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("messages"),
        "{body}"
    );
}

#[tokio::test]
async fn empty_tools_array_is_not_rejected() {
    // 很多 OpenAI 客户端默认发 tools: []，不能当成「要调用工具」
    for path in ["/v1/responses", "/v1/messages", "/v1/chat/completions"] {
        let mut body = match path {
            "/v1/responses" => json!({"input": "hi"}),
            "/v1/messages" => json!({"max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]}),
            _ => json!({"messages": [{"role": "user", "content": "hi"}]}),
        };
        body["tools"] = json!([]);
        let (status, response) = post(path, body, Some(TEST_KEY)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} 带空 tools 数组不应被拒：{response}"
        );
    }
}

#[tokio::test]
async fn messages_tools_error_uses_anthropic_shape() {
    let (status, body) = post(
        "/v1/messages",
        json!({
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "f", "input_schema": {}}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // Anthropic 客户端只认 {"type":"error","error":{...}}
    assert_eq!(body["type"], "error", "必须是 Anthropic 错误格式：{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("不支持工具调用"),
        "{body}"
    );
    // 关键：不能是 OpenAI 形状（那会多一层 error.code）
    assert!(body["error"]["code"].is_null(), "不应出现 OpenAI 的 code 字段：{body}");
}

#[tokio::test]
async fn chat_rejects_tools() {
    let (status, body) = post(
        "/v1/chat/completions",
        json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f"}}],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "tools_not_supported", "{body}");
}

#[tokio::test]
async fn tools_error_is_openai_shape_on_openai_paths() {
    let (status, body) = post(
        "/v1/responses",
        json!({"input": "hi", "tools": [{"type": "function", "name": "f"}]}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // OpenAI 路径：{"error":{...}}，没有顶层 "type"
    assert_eq!(body["error"]["code"], "tools_not_supported", "{body}");
    assert!(body["type"].is_null(), "OpenAI 路径不应带顶层 type：{body}");
}

// ---------------- /v1/messages/count_tokens ----------------

/// 带 `x-api-key` 头发 POST —— Anthropic 客户端就是这么发的。
async fn post_with_x_api_key(path: &str, body: Value, key: &str) -> (StatusCode, Value) {
    let response = build_router(state())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("x-api-key", key)
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
async fn count_tokens_requires_auth() {
    let (status, body) = post(
        "/v1/messages/count_tokens",
        json!({"messages": [{"role": "user", "content": "hi"}]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["type"], "error", "401 也应是 Anthropic 错误体：{body}");
    assert_eq!(body["error"]["type"], "authentication_error", "{body}");
}

#[tokio::test]
async fn count_tokens_matches_the_messages_usage_count() {
    let (status, body) = post(
        "/v1/messages/count_tokens",
        json!({"model": "text", "messages": [{"role": "user", "content": "hello"}]}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 与 core::tokens 的同一条口径：1(role) + 1(hello) + 3(结构) + 3(引导)
    assert_eq!(body["input_tokens"], 8, "{body}");
    assert_eq!(body.as_object().expect("对象").len(), 1, "只该返回 input_tokens：{body}");
}

#[tokio::test]
async fn count_tokens_counts_system_and_block_content() {
    let plain = post(
        "/v1/messages/count_tokens",
        json!({"messages": [{"role": "user", "content": "第一问"}]}),
        Some(TEST_KEY),
    )
    .await;
    let rich = post(
        "/v1/messages/count_tokens",
        json!({
            "system": [{"type": "text", "text": "你是助手"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "第一问"}]},
                {"role": "assistant", "content": "第一答"},
                {"role": "user", "content": "第二问"}
            ],
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(plain.0, StatusCode::OK, "{}", plain.1);
    assert_eq!(rich.0, StatusCode::OK, "{}", rich.1);
    let plain_tokens = plain.1["input_tokens"].as_i64().expect("数字");
    let rich_tokens = rich.1["input_tokens"].as_i64().expect("数字");
    assert!(rich_tokens > plain_tokens, "system 与更多消息必须计入：{rich_tokens} vs {plain_tokens}");
}

#[tokio::test]
async fn count_tokens_rejects_empty_messages() {
    let (status, body) = post(
        "/v1/messages/count_tokens",
        json!({"messages": []}),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error", "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("messages"),
        "{body}"
    );
}

#[tokio::test]
async fn count_tokens_rejects_tools() {
    let (status, body) = post(
        "/v1/messages/count_tokens",
        json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "get_weather"}]
        }),
        Some(TEST_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], "error", "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("工具"),
        "{body}"
    );
}

#[tokio::test]
async fn count_tokens_accepts_x_api_key() {
    let (status, body) = post_with_x_api_key(
        "/v1/messages/count_tokens",
        json!({"messages": [{"role": "user", "content": "hello"}]}),
        TEST_KEY,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["input_tokens"], 8, "{body}");
}

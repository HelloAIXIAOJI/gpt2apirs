//! `/v1/images/generations` 的**参数校验**测试。
//!
//! 全部离线：只验证「拒绝非法请求」这一层，不触发真实上游调用。

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

fn state(access_token: Option<&'static str>) -> Arc<AppState> {
    let mut map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("AUTH_KEY", AUTH_KEY),
    ]);
    if let Some(token) = access_token {
        map.insert("GPT2APIRS_ACCESS_TOKEN", token);
    }
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

async fn get_json(state: Arc<AppState>, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state)
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn post_image(state: Arc<AppState>, body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state)
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
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

fn assert_openai_error(body: &Value, code: &str) {
    assert!(body["error"].is_object(), "应为 OpenAI 错误格式: {body}");
    assert_eq!(body["error"]["code"], code, "错误码不符: {body}");
}

fn account_pool(db: &sqlx::PgPool) -> Arc<gpt2apirs_account::AccountPool> {
    Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(db.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ))
}

#[tokio::test]
async fn requires_auth() {
    let (status, body) = post_image(state(None), serde_json::json!({"prompt": "x"}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_openai_error(&body, "invalid_api_key");
}

#[tokio::test]
async fn rejects_empty_prompt() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "   "}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn rejects_n_out_of_range() {
    for n in [0, 5, 100] {
        let (status, body) = post_image(
            state(None),
            serde_json::json!({"prompt": "a cat", "n": n}),
            Some(AUTH_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "n={n} 应被拒绝");
        assert_eq!(body["error"]["param"], "n", "n={n}");
    }
}

#[tokio::test]
async fn rejects_bad_response_format() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "response_format": "xml"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "response_format");
}

#[tokio::test]
async fn rejects_malformed_size() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "size": "big"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["param"], "size", "错误应指向 size: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("宽x高"),
        "应说明正确写法: {body}"
    );
}

#[tokio::test]
async fn rejects_unknown_quality() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "quality": "ultra"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["param"], "quality", "错误应指向 quality: {body}");
}

#[tokio::test]
async fn accepts_auto_size() {
    // auto 表示「不干预」，不该被当成非法值；池空 → 503 说明过了参数校验
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "size": "auto"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn rejects_stream() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "stream": true}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "stream");
    // 拒绝要指路：上游没有逐块图片流，异步任务才是边等边查的正路
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("image-tasks") && message.contains("sediment"),
        "错误信息应说明原因并指路：{message}"
    );
}

#[tokio::test]
async fn empty_pool_returns_503_no_available_account() {
    // 账号池为空时必须返回 503 而不是 500：
    // 「暂时没有资源」不是「程序出错」，语义不同（设计文档 D11）。
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("/api/accounts"),
        "错误信息应指引用户去导入账号: {body}"
    );
}

#[tokio::test]
async fn rejects_malformed_json() {
    let response = build_router(state(None))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {AUTH_KEY}"))
                .body(Body::from("{not json"))
                .expect("request"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn models_requires_auth() {
    let (status, body) = get_json(state(None), "/v1/models", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_openai_error(&body, "invalid_api_key");
}

#[tokio::test]
async fn models_lists_image_model_and_keeps_text_model_out_of_image_validation() {
    let (status, body) = get_json(state(None), "/v1/models", Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().expect("data 应为数组");
    let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"gpt-image-2"), "图片模型必须出现在目录里：{ids:?}");
    // 目录里的文本模型不能反过来变成合法的图片模型
    let text_ids: Vec<&str> = data
        .iter()
        .filter(|m| m["kind"] == "text")
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(!text_ids.is_empty(), "目录应包含文本模型");
    for id in text_ids {
        let (status, rejected) = post_image(
            state(None),
            serde_json::json!({"prompt": "a cat", "model": id}),
            Some(AUTH_KEY),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "文本模型 {id} 不该被图片接口接受：{rejected}"
        );
    }
    for item in data {
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "gpt2apirs");
    }
}

#[tokio::test]
async fn rejects_unsupported_model() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "model": "dall-e-3"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "model");
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("不支持"),
        "{body}"
    );
}

#[tokio::test]
async fn rejects_codex_model_with_specific_hint() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "model": "codex-gpt-image-2"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"].as_str().unwrap_or("").contains("Codex"),
        "应提示需要 Codex 链路: {body}"
    );
}

#[tokio::test]
async fn rejects_bad_output_format() {
    let (status, body) = post_image(
        state(None),
        serde_json::json!({"prompt": "a cat", "output_format": "bmp"}),
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "不支持的输出格式必须拒绝");
    assert_eq!(body["error"]["param"], "output_format");
}

#[tokio::test]
async fn accepts_gif_output_format() {
    // 校验层必须放行；测试环境账号池为空，所以走到池子就返回 503 而不是 400
    let (status, body) = post_image(
        state(None),
        serde_json::json!({
            "prompt": "a rotating cube",
            "output_format": "gif",
            "frame_count": 20,
            "frame_duration_ms": 120
        }),
        Some(AUTH_KEY),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "gif 不该被参数校验拒绝: {body}");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

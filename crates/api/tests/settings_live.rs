//! `/api/settings` 的真机测试：证明「内容粗筛可配置」这个缺陷真的修好了。
//!
//! 需要 `.env` 里的 DATABASE_URL，缺则自动跳过。
//!
//! **这条测试不需要上游可达** —— 它验证的是「配置生效」而不是「模型回复」，
//! 所以即使在断网环境也能证明修复有效。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_storage::repo::PgSettingsRepo;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEYWORD: &str = "gpt2apirs-粗筛测试词";

/// 从真实 env 读出的登录口令（后台接口用它；与调用 /v1 的 API key 不同）。
static ADMIN_KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// 预置给 /v1 用的 API key（与 AUTH_KEY 同值，模拟旧部署迁移后的 legacy key）。
static API_KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn admin_key() -> &'static str {
    ADMIN_KEY.get().map(String::as_str).unwrap_or("")
}

fn api_key() -> &'static str {
    API_KEY.get().map(String::as_str).unwrap_or("")
}

fn prepare_env() -> bool {
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
    std::env::var("DATABASE_URL")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

async fn state() -> Option<Arc<AppState>> {
    let config = Arc::new(BootstrapConfig::from_env().ok()?);
    let _ = ADMIN_KEY.set(config.admin_key.clone());
    let pool = gpt2apirs_storage::pool::build_pool(&config).await.ok()?;
    let repo = PgSettingsRepo::new(pool.clone());
    // 预置一把与配置里 AUTH_KEY 同值的 API key，供 /v1/* 调用
    let test_key = config.auth_key.clone();
    let _ = API_KEY.set(test_key.clone());
    let snapshot = repo.load().await.ok()?;
    let settings = Arc::new(RuntimeSettings::new(snapshot.data));
    Some(Arc::new(AppState::new(
        config,
        settings,
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
        // 调用 /v1/* 用 API key：测试里预置一把与 AUTH_KEY 相同的 key
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(&test_key, "test"),
        )),
    )))
}

async fn call(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
                .header(
            "authorization",
            format!(
                "Bearer {}",
                if path.starts_with("/v1/") { api_key() } else { admin_key() }
            ),
        );
    let body = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).expect("序列化")),
        None => Body::empty(),
    };
    let response = build_router(state.clone())
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn chat_body(text: &str) -> Value {
    json!({
        "model": "gpt-5-5",
        "messages": [{ "role": "user", "content": text }],
    })
}

#[tokio::test]
async fn keyword_setting_actually_takes_effect() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let Some(state) = state().await else {
        eprintln!("跳过：无法连接数据库");
        return;
    };

    // ---- 0. 先确保起点是空的 ----
    let _ = call(
        &state,
        "PUT",
        "/api/settings",
        Some(json!({"values": {"chat_blocked_keywords": ""}})),
    )
    .await;

    // ---- 1. 未配置时：带「关键词」的请求不会被内容策略拦 ----
    let (status, body) = call(
        &state,
        "POST",
        "/v1/chat/completions",
        Some(chat_body(&format!("这句话里有 {KEYWORD}"))),
    )
    .await;
    assert_ne!(
        body["error"]["code"].as_str(),
        Some("content_policy_violation"),
        "未配置关键词时不应拦截：{status} {body}"
    );

    // ---- 2. 写入关键词 ----
    let (status, body) = call(
        &state,
        "PUT",
        "/api/settings",
        Some(json!({"values": {"chat_blocked_keywords": KEYWORD}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "写入设置应成功：{body}");

    // ---- 3. 读回来确认已持久化 ----
    let (status, body) = call(&state, "GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    let stored = body["strings"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["key"] == "chat_blocked_keywords"))
        .and_then(|item| item["value"].as_str())
        .unwrap_or("");
    assert_eq!(stored, KEYWORD, "读回的设置应与写入一致");

    // ---- 4. 同一个请求现在必须被拦（这就是缺陷修复的核心证据）----
    let (status, body) = call(
        &state,
        "POST",
        "/v1/chat/completions",
        Some(chat_body(&format!("这句话里有 {KEYWORD}"))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"].as_str(),
        Some("content_policy_violation"),
        "配置后应命中粗筛：{body}"
    );

    // ---- 5. 不相关的请求仍然放行 ----
    let (status, body) = call(
        &state,
        "POST",
        "/v1/chat/completions",
        Some(chat_body("一句完全正常的话")),
    )
    .await;
    assert_ne!(
        body["error"]["code"].as_str(),
        Some("content_policy_violation"),
        "无关请求不应被拦：{status} {body}"
    );

    // ---- 6. 还原，别把测试关键词留在生产设置里 ----
    let (status, _) = call(
        &state,
        "PUT",
        "/api/settings",
        Some(json!({"values": {"chat_blocked_keywords": ""}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "清理应成功");

    let (_, body) = call(&state, "GET", "/api/settings", None).await;
    let restored = body["strings"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["key"] == "chat_blocked_keywords"))
        .and_then(|item| item["value"].as_str())
        .unwrap_or("x");
    assert_eq!(restored, "", "测试后必须还原为空串");
}

#[tokio::test]
async fn numeric_setting_is_clamped_on_write() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let Some(state) = state().await else {
        eprintln!("跳过：无法连接数据库");
        return;
    };

    // 写一个越界值：3 是上限，这里写 999
    let (status, body) = call(
        &state,
        "PUT",
        "/api/settings",
        Some(json!({"values": {"image_account_concurrency": 999}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let value = body["numerics"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["key"] == "image_account_concurrency"))
        .and_then(|item| item["value"].as_i64())
        .expect("应返回该项");
    assert_eq!(value, 3, "越界值应被钳到上限 3，而不是原样写入");

    // 还原为默认值
    let _ = call(
        &state,
        "PUT",
        "/api/settings",
        Some(json!({"values": {"image_account_concurrency": 1}})),
    )
    .await;
}

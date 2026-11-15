//! `DELETE /api/accounts/{id}` 的路由契约测试（离线）。
//!
//! 回归的是这个缺陷：路径 id 曾被完全忽略 —— handler 只读 JSON body，
//! 所以不带 body 的 `DELETE /api/accounts/{id}` 返回 415，
//! 带 `{"ids":[]}` 返回 400，而那个 id 从头到尾没被看过一眼。
//!
//! 离线跑：DB 指向不可达地址，因此「请求已经被 handler 接住」的证明是
//! **不是** 415 / 400-from-json，而是走进了仓库层（500）。

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

const ADMIN_KEY: &str = "admin-key-for-delete-tests";

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
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

/// 发 DELETE，可选 body 与 content-type。
async fn send_delete(path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("DELETE")
        .uri(path)
        .header("authorization", format!("Bearer {ADMIN_KEY}"));
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).expect("序列化"))
        }
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

/// 核心回归：不带 body 也必须进到 handler，而不是被 JSON 提取器挡成 415。
#[tokio::test]
async fn path_delete_without_body_is_not_415() {
    let id = Uuid::new_v4();
    let (status, _) = send_delete(&format!("/api/accounts/{id}"), None).await;
    assert_ne!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "路径删除不该要求请求体（缺陷原症状）"
    );
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "不该因为 body 里没 ids 就 400"
    );
}

/// 空 body 时确实走到了仓库层：惰性连接池连不上 → 500。
#[tokio::test]
async fn path_delete_reaches_repository() {
    let id = Uuid::new_v4();
    let (status, body) = send_delete(&format!("/api/accounts/{id}"), None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["detail"]["message"].is_string() || body["error"]["message"].is_string(),
        "错误体应有可读信息：{body}"
    );
    assert!(
        !body.to_string().contains("nobody"), 
        "不该把数据库口令里的用户名泄进错误体：{body}"
    );
}

/// 批量端点仍然只认 body：给空 ids 必须 400，而不是「删光」。
///
/// 错误体形状这里两种都接受：accounts 路由目前回 OpenAI 形状，
/// 其余管理面回 `{"detail":…}` —— 那是另一个待收敛的问题，不该由本测试锁死。
#[tokio::test]
async fn batch_delete_still_rejects_empty_ids() {
    let (status, body) = send_delete("/api/accounts", Some(serde_json::json!({ "ids": [] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["detail"]["message"]
        .as_str()
        .or_else(|| body["error"]["message"].as_str())
        .unwrap_or("");
    assert!(message.contains("ids") || message.contains("不能为空"), "{body}");
}

/// 批量端点不带 body 也不该 415（它本来就要求 body，这里锁的是错误形状不变）。
#[tokio::test]
async fn batch_delete_requires_body() {
    let (status, _) = send_delete("/api/accounts", None).await;
    assert!(
        status == StatusCode::UNSUPPORTED_MEDIA_TYPE || status == StatusCode::BAD_REQUEST,
        "批量端点没有 body 应被拒，实际 {status}"
    );
}

/// 路径不是合法 UUID 时由 axum 直接拒（400），不会走到仓库。
#[tokio::test]
async fn path_delete_rejects_bad_uuid() {
    let (status, _) = send_delete("/api/accounts/not-a-uuid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

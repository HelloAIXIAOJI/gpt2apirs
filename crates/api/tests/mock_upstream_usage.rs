//! 端到端 usage 口径回归：mock 上游 + 真库账号池。
//!
//! # 钉住的那条不变量
//!
//! 同一个请求，三个口径必须给出同一个 `input_tokens`：
//!
//! - `POST /v1/messages/count_tokens`（预估）
//! - `POST /v1/messages` 非流式的 `usage.input_tokens`
//! - `POST /v1/messages` 流式 `message_start.message.usage.input_tokens`
//!
//! 曾经它们不一致：流式把内部注入的富文本 guard 提示词也算进了用户账单
//! （实测 149 vs 77）。离线用例覆盖不到这个组合，只有端到端能抓。
//!
//! # 条件
//!
//! 需要 `--features tls`（要真发 HTTP）与 `DATABASE_URL`（账号池读库）。
//! 两者缺一则跳过。
//!
//! # 数据安全
//!
//! 账号 token 带独立前缀，只删自己的行。

#![cfg(feature = "tls")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_storage::repo::{ApiKeyStore, MemoryApiKeys};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const API_KEY: &str = "sk-g2a-usage-regression-00000000";
const TEST_PREFIX: &str = "gpt2apirs-usage-test-";

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

async fn setup() -> Option<(Arc<AppState>, PgPool)> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let config = Arc::new(BootstrapConfig::from_env().expect("配置应可加载"));
    let pool = gpt2apirs_storage::pool::build_pool(&config)
        .await
        .expect("应能连接 PostgreSQL");
    gpt2apirs_storage::migrate::run_migrations(&pool)
        .await
        .expect("迁移应成功");

    let cleanup = |pool: PgPool| async move {
        let _ = sqlx::query("delete from accounts where access_token like $1")
            .bind(format!("{TEST_PREFIX}%"))
            .execute(&pool)
            .await;
    };
    cleanup(pool.clone()).await;

    // 账号池要有一个文本可用的账号：token 内容无所谓，链路全在本地 mock
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let new = gpt2apirs_account::repo::NewAccount {
        access_token: format!("{TEST_PREFIX}usage"),
        email: Some("usage@test.local".to_string()),
        quota: 1,
        quota_confirmed: true,
        ..Default::default()
    };
    repo.upsert(&new).await.expect("应能建测试账号");

    let state = Arc::new(AppState::new(
        config,
        Arc::new(RuntimeSettings::new(std::collections::HashMap::new())),
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
        Arc::new(ApiKeyStore::Memory(
            MemoryApiKeys::new().with_key(API_KEY, "usage regression"),
        )),
    ));
    state.pool.reload().await.ok();
    Some((state, pool))
}

/// 发请求，拿 (状态码, 响应文本)。
async fn send(state: &Arc<AppState>, path: &str, body: Value) -> (StatusCode, String) {
    let response = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {API_KEY}"))
                .body(Body::from(body.to_string()))
                .expect("构造请求"),
        )
        .await
        .expect("请求应成功");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读 body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn message_start_usage(sse: &str) -> Option<i64> {
    for line in sse.lines() {
        let Some(payload) = line.strip_prefix("data: ") else { continue };
        let Ok(value) = serde_json::from_str::<Value>(payload) else { continue };
        if value["type"] == "message_start" {
            return value["message"]["usage"]["input_tokens"].as_i64();
        }
    }
    None
}

#[tokio::test]
async fn usage_matches_across_count_stream_and_non_stream() {
    let mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = setup().await else { return };

    let body = json!({
        "model": "gpt-5-5",
        "max_tokens": 64,
        "system": "回答要简洁。",
        "messages": [
            {"role": "user", "content": "第一个问题"},
            {"role": "assistant", "content": "第一个回答"},
            {"role": "user", "content": "第二个问题"}
        ]
    });

    // 1) 预估
    let (status, raw) = send(&state, "/v1/messages/count_tokens", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let estimated: i64 = serde_json::from_str::<Value>(&raw).expect("JSON")["input_tokens"]
        .as_i64()
        .expect("input_tokens");

    // 2) 非流式
    let (status, raw) = send(&state, "/v1/messages", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let non_stream = serde_json::from_str::<Value>(&raw).expect("JSON")["usage"]["input_tokens"]
        .as_i64()
        .expect("usage.input_tokens");

    // 3) 流式
    let mut streaming_body = body.clone();
    streaming_body["stream"] = json!(true);
    let (status, sse) = send(&state, "/v1/messages", streaming_body).await;
    assert_eq!(status, StatusCode::OK, "{sse}");
    let streaming = message_start_usage(&sse).expect("流式应有 message_start.usage");

    assert_eq!(
        estimated, non_stream,
        "预估与非流式 usage 不一致：{estimated} vs {non_stream}"
    );
    assert_eq!(
        non_stream, streaming,
        "流式 usage 与非流式不一致（历史上就是这里把 guard 提示词算进了账单）：\
         非流式 {non_stream}，流式 {streaming}"
    );

    assert!(
        mock.saw("/backend-api/f/conversation"),
        "三次请求应真的打到 mock 上游，实际收到 {:?}",
        mock.hits()
    );

    let _ = sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(&pool)
        .await;
}

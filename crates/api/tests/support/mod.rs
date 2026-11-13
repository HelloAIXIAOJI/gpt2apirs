//! api 层端到端测试的共享支撑：真库 + mock 上游 + 请求发送。
//!
//! 需要 `--features tls`（要真发 HTTP）与 `DATABASE_URL`（账号池读库）。
//! 缺任一条件时 `setup()` 返回 `None`，用例直接 return 当作跳过。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_storage::repo::{ApiKeyStore, MemoryApiKeys};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

/// 测试用的 API key（内存 key store 里预置）。
pub const API_KEY: &str = "sk-g2a-usage-regression-00000000";

/// 账号 token 前缀：只删自己的行。
pub const TEST_PREFIX: &str = "gpt2apirs-apitest-";

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

/// 清理本测试创建的账号。
pub async fn cleanup(pool: &PgPool) {
    let _ = sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await;
}

/// 建好带一个文本可用账号的状态与连接池；缺条件返回 `None`。
pub async fn setup() -> Option<(Arc<AppState>, PgPool)> {
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
    cleanup(&pool).await;

    // 账号池要有一个文本可用的账号：token 内容无所谓，链路全在本地 mock
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let new = gpt2apirs_account::repo::NewAccount {
        access_token: format!("{TEST_PREFIX}account"),
        email: Some("apitest@test.local".to_string()),
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
            MemoryApiKeys::new().with_key(API_KEY, "api test"),
        )),
    ));
    state.pool.reload().await.ok();
    Some((state, pool))
}

/// 发一个带 API key 的 POST，返回 (状态码, 响应文本)。
pub async fn send(state: &Arc<AppState>, path: &str, body: Value) -> (StatusCode, String) {
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

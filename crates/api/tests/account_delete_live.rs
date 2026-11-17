//! `DELETE /api/accounts/{id}` 的真实链路测试。
//!
//! 需要 .env 里的 DATABASE_URL，缺则自动跳过。这条缺陷的本质只有真库能测：
//! 路径 id 曾经被忽略，所以「删的是不是那一个」必须看库里的行。
//!
//! # 数据安全
//!
//! 账号 token 一律带 gpt2apirs-test- 前缀，只删自己的行。

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
use uuid::Uuid;

// 刻意不用 `gpt2apirs-test-`：其余 live 测试的 clean() 按该前缀扫库，
// 并行跑时会把这里的账号一起删掉（实测报 404 而非 200）。
const TEST_PREFIX: &str = "gpt2apirs-del-test-";
const API_KEY: &str = "sk-g2a-test-only-0000000000000000";

static ADMIN_KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn admin_key() -> &'static str {
    ADMIN_KEY.get().map(String::as_str).unwrap_or("")
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

async fn state() -> Option<(Arc<AppState>, PgPool)> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let config = Arc::new(BootstrapConfig::from_env().expect("配置应可加载"));
    let _ = ADMIN_KEY.set(config.admin_key.clone());
    let pool = gpt2apirs_storage::pool::build_pool(&config)
        .await
        .expect("应能连接 PostgreSQL");
    gpt2apirs_storage::migrate::run_migrations(&pool)
        .await
        .expect("迁移应成功");
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
            MemoryApiKeys::new().with_key(API_KEY, "test"),
        )),
    ));
    Some((state, pool))
}

async fn send(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", admin_key()));
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("构造请求"),
        None => builder.body(Body::empty()).expect("构造请求"),
    };
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("请求应成功");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读 body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 每个用例一个独立命名空间。
///
/// 三条用例并行跑时共用一个前缀会互相清理（实测：批量用例的 keeper 被别的用例删掉），
/// 所以清理必须按用例收窄，而不是按整个前缀扫。
fn scope(case: &str) -> String {
    format!("{TEST_PREFIX}{case}-")
}

async fn clean(pool: &PgPool, case: &str) {
    sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{}%", scope(case)))
        .execute(pool)
        .await
        .expect("清理测试账号失败");
}

async fn make_account(pool: &PgPool, case: &str, suffix: &str) -> Uuid {
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let new = gpt2apirs_account::repo::NewAccount {
        access_token: format!("{}{suffix}", scope(case)),
        email: Some(format!("{suffix}@test.local")),
        quota: 1,
        quota_confirmed: true,
        ..Default::default()
    };
    repo.upsert(&new).await.expect("建账号").account().id
}

async fn exists(pool: &PgPool, id: Uuid) -> bool {
    let count: i64 = sqlx::query_scalar("select count(*) from accounts where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("查账号");
    count > 0
}

/// 核心回归：`DELETE /api/accounts/{id}` 只删路径里的那个，另一个必须还在。
#[tokio::test]
async fn path_delete_removes_only_the_named_account() {
    let Some((state, pool)) = state().await else { return };
    let case = "only-named";
    clean(&pool, case).await;

    let target = make_account(&pool, case, "target").await;
    let bystander = make_account(&pool, case, "bystander").await;

    let (status, body) = send(&state, "DELETE", &format!("/api/accounts/{target}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], 1, "{body}");
    assert_eq!(body["id"], target.to_string(), "{body}");

    assert!(!exists(&pool, target).await, "目标账号应被删除");
    assert!(exists(&pool, bystander).await, "不该误删别的账号");

    clean(&pool, case).await;
}

/// 不存在的 id 返回 404，而不是静默成功。
#[tokio::test]
async fn path_delete_missing_account_is_404() {
    let Some((state, pool)) = state().await else { return };
    let case = "missing";
    clean(&pool, case).await;

    let missing = Uuid::new_v4();
    let (status, body) = send(&state, "DELETE", &format!("/api/accounts/{missing}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    clean(&pool, case).await;
}

/// 批量端点仍然按 body 删，且不影响路径语义。
#[tokio::test]
async fn batch_delete_still_works_by_ids() {
    let Some((state, pool)) = state().await else { return };
    let case = "batch";
    clean(&pool, case).await;

    let a = make_account(&pool, case, "a").await;
    let b = make_account(&pool, case, "b").await;
    let keeper = make_account(&pool, case, "keeper").await;

    let (status, body) = send(
        &state,
        "DELETE",
        "/api/accounts",
        Some(serde_json::json!({ "ids": [a, b] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], 2, "{body}");

    assert!(!exists(&pool, a).await);
    assert!(!exists(&pool, b).await);
    assert!(exists(&pool, keeper).await);

    clean(&pool, case).await;
}

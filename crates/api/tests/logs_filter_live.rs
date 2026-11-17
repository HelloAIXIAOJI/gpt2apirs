//! 日志错误码过滤的真实链路测试。
//!
//! 需要 DATABASE_URL，缺则跳过。筛选条件最终落在 SQL 上，
//! mock 断言不出来「占位符编号与绑定顺序是否一致」，所以走真库。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_storage::repo::{ApiKeyStore, CallRecordRepo, MemoryApiKeys, NewCallRecord};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

const PATH_PREFIX: &str = "/gpt2apirs-test-logs-";
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
    let settings = Arc::new(RuntimeSettings::new(std::collections::HashMap::new()));
    let state = Arc::new(AppState::new(
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
        Arc::new(ApiKeyStore::Memory(
            MemoryApiKeys::new().with_key(API_KEY, "test"),
        )),
    ));
    Some((state, pool))
}

async fn send(state: &Arc<AppState>, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", admin_key()))
        .body(Body::empty())
        .expect("构造请求");
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

async fn clean(pool: &PgPool) {
    let _ = sqlx::query("delete from call_records where path like $1")
        .bind(format!("{PATH_PREFIX}%"))
        .execute(pool)
        .await;
}

#[tokio::test]
async fn code_filter_returns_only_matching_rows() {
    let Some((state, pool)) = state().await else { return };
    clean(&pool).await;

    let repo = CallRecordRepo::new(pool.clone());
    for (suffix, code) in [("alpha", "g2a_test_error_a"), ("beta", "g2a_test_error_b")] {
        repo.insert(&NewCallRecord {
            method: "GET".to_string(),
            path: format!("{PATH_PREFIX}{suffix}"),
            status: 400,
            duration_ms: 5,
            account_id: None,
            request_id: None,
            error_code: Some(code.to_string()),
        })
        .await
        .expect("写日志");
    }

    let (status, body) = send(&state, "/api/logs?limit=20&code=g2a_test_error_a").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data 应是数组").clone();
    let mine: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            row["path"]
                .as_str()
                .map(|path| path.starts_with(PATH_PREFIX))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(mine.len(), 1, "应只剩匹配错误码的那一条: {body}");
    assert_eq!(mine[0]["error_code"], "g2a_test_error_a", "{body}");

    let (status, body) = send(&state, "/api/logs?limit=20&code=g2a_test_no_such_code").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data 应是数组");
    assert!(
        !rows.iter().any(|row| row["path"]
            .as_str()
            .map(|path| path.starts_with(PATH_PREFIX))
            .unwrap_or(false)),
        "不存在的错误码不该命中: {body}"
    );

    clean(&pool).await;
}

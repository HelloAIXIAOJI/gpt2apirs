//! 代理分组接口的真实链路测试。
//!
//! 需要 .env 里的 DATABASE_URL，缺则自动跳过。走真实 PostgreSQL，
//! 因为分组路由的语义就在「组地址同步进账号」这件事上，lazy pool 测不出来。
//!
//! # 数据安全
//!
//! 分组名与账号 token 都带 gpt2apirs-test- 前缀，只删自己的行。

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
use uuid::Uuid;

const TEST_PREFIX: &str = "gpt2apirs-test-";
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
        // 预置一把 API key：用来证明它进不了后台接口。
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
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
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
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn clean(pool: &PgPool) {
    sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await
        .expect("清理测试账号失败");
    sqlx::query("delete from proxy_groups where name like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await
        .expect("清理测试分组失败");
}

async fn make_account(pool: &PgPool, suffix: &str) -> Uuid {
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let new = gpt2apirs_account::repo::NewAccount {
        access_token: format!("{TEST_PREFIX}{suffix}"),
        email: Some(format!("{suffix}@test.local")),
        quota: 1,
        quota_confirmed: true,
        ..Default::default()
    };
    repo.upsert(&new).await.expect("建账号").account().id
}

async fn account_row(pool: &PgPool, id: Uuid) -> (String, Option<Uuid>) {
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let account = repo
        .get_by_id(id)
        .await
        .expect("查账号")
        .expect("账号应存在");
    (account.proxy, account.proxy_group_id)
}

#[tokio::test]
async fn admin_credential_required_and_api_key_rejected() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();

    let (status, _) = send(&state, "GET", "/api/proxy-groups", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "没有凭据必须 401");

    let (status, body) = send(&state, "GET", "/api/proxy-groups", Some(API_KEY), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "API key 不能进后台接口，实际 body: {body}"
    );

    let (status, body) = send(
        &state,
        "GET",
        "/api/proxy-groups",
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "登录口令应被接受: {body}");
    assert!(body["data"].is_array(), "应返回 data 数组: {body}");

    clean(&pool).await;
}

#[tokio::test]
async fn create_blank_name_is_rejected() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();
    clean(&pool).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/proxy-groups",
        Some(admin_key()),
        Some(json!({ "name": "   ", "url": "http://127.0.0.1:1080" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空名称应 400: {body}");

    let (status, body) = send(
        &state,
        "GET",
        "/api/proxy-groups",
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let empty = body["data"]
        .as_array()
        .expect("数组")
        .iter()
        .all(|group| !group["name"].as_str().unwrap_or("").trim().is_empty());
    assert!(empty, "不应建出空名分组: {body}");

    clean(&pool).await;
}

#[tokio::test]
async fn update_group_url_syncs_member_accounts() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();
    clean(&pool).await;

    let name = format!("{TEST_PREFIX}sync");
    let (status, created) = send(
        &state,
        "POST",
        "/api/proxy-groups",
        Some(admin_key()),
        Some(json!({ "name": name, "url": "http://127.0.0.1:1080" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建组应成功: {created}");
    let group_id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid");

    let a = make_account(&pool, "route-sync-a").await;
    let b = make_account(&pool, "route-sync-b").await;
    for id in [a, b] {
        let (status, body) = send(
            &state,
            "POST",
            &format!("/api/accounts/{id}/proxy-group"),
            Some(admin_key()),
            Some(json!({ "group_id": group_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "入组应成功: {body}");
        assert_eq!(body["proxy"], "http://127.0.0.1:1080", "入组应复制组地址");
        assert_eq!(
            body["proxy_group_id"].as_str(),
            Some(group_id.to_string().as_str())
        );
    }

    let (status, listed) = send(
        &state,
        "GET",
        "/api/proxy-groups",
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view = listed["data"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|group| group["id"] == json!(group_id.to_string()))
        .expect("列表应含该组")
        .clone();
    assert_eq!(view["members"], 2, "成员数应为 2: {listed}");

    // 只改名字：不该触发同步
    let (status, renamed) = send(
        &state,
        "PUT",
        &format!("/api/proxy-groups/{group_id}"),
        Some(admin_key()),
        Some(json!({ "name": format!("{name}-renamed"), "url": "http://127.0.0.1:1080" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改名应成功: {renamed}");
    assert_eq!(renamed["synced_accounts"], 0, "地址没变不该同步账号");

    // 改地址：成员必须跟着变
    let (status, updated) = send(
        &state,
        "PUT",
        &format!("/api/proxy-groups/{group_id}"),
        Some(admin_key()),
        Some(json!({ "name": format!("{name}-renamed"), "url": "http://127.0.0.1:2080" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改地址应成功: {updated}");
    assert_eq!(updated["synced_accounts"], 2, "应同步两个成员");
    for id in [a, b] {
        assert_eq!(
            account_row(&pool, id).await.0,
            "http://127.0.0.1:2080",
            "成员账号的 proxy 应被重写"
        );
    }

    clean(&pool).await;
}

#[tokio::test]
async fn assign_to_missing_group_is_400() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();
    clean(&pool).await;

    let id = make_account(&pool, "route-missing-group").await;
    let (status, body) = send(
        &state,
        "POST",
        &format!("/api/accounts/{id}/proxy-group"),
        Some(admin_key()),
        Some(json!({ "group_id": Uuid::new_v4() })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "不存在的分组应 400: {body}");
    assert_eq!(
        account_row(&pool, id).await,
        (String::new(), None),
        "失败的入组不能留下痕迹"
    );

    clean(&pool).await;
}

#[tokio::test]
async fn delete_group_detaches_members_and_keeps_proxy() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();
    clean(&pool).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/proxy-groups",
        Some(admin_key()),
        Some(json!({ "name": format!("{TEST_PREFIX}delete"), "url": "http://127.0.0.1:1080" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let group_id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid");

    let id = make_account(&pool, "route-delete").await;
    let (status, _) = send(
        &state,
        "POST",
        &format!("/api/accounts/{id}/proxy-group"),
        Some(admin_key()),
        Some(json!({ "group_id": group_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &state,
        "DELETE",
        &format!("/api/proxy-groups/{group_id}"),
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "删组应成功: {body}");
    assert_eq!(body["detached_accounts"], 1, "应摘掉一个成员");

    assert_eq!(
        account_row(&pool, id).await,
        ("http://127.0.0.1:1080".to_string(), None),
        "删组后账号保留原代理串、不再挂组"
    );

    let (status, body) = send(
        &state,
        "DELETE",
        &format!("/api/proxy-groups/{group_id}"),
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "重复删组应 400: {body}");

    clean(&pool).await;
}

#[tokio::test]
async fn test_group_reports_egress_result() {
    let Some((state, pool)) = state().await else { return };
    let _guard = lock();
    clean(&pool).await;

    // 直连：能不能连出去取决于环境，所以只断言接口本身正常返回结构
    let (status, created) = send(
        &state,
        "POST",
        "/api/proxy-groups",
        Some(admin_key()),
        Some(json!({ "name": format!("{TEST_PREFIX}probe"), "url": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let direct_id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid");

    let (status, body) = send(
        &state,
        "POST",
        &format!("/api/proxy-groups/{direct_id}/test"),
        Some(admin_key()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "探活接口应返回 200: {body}");
    assert!(body["ok"].is_boolean(), "应带 ok 字段: {body}");
    assert!(body["duration_ms"].is_number(), "应带耗时: {body}");
    assert_eq!(body["proxy"], "(直连)", "空代理串应标成直连: {body}");

    // 代理串非法：参数错误，不能当成 500
    let (status, broken) = send(
        &state,
        "POST",
        "/api/proxy-groups",
        Some(admin_key()),
        Some(json!({ "name": format!("{TEST_PREFIX}probe-bad"), "url": "///" })),
    )
    .await;
    if status == StatusCode::OK {
        let bad_id = Uuid::parse_str(broken["id"].as_str().expect("id")).expect("uuid");
        let (status, body) = send(
            &state,
            "POST",
            &format!("/api/proxy-groups/{bad_id}/test"),
            Some(admin_key()),
            None,
        )
        .await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::OK,
            "非法代理不应 500: {status} {body}"
        );
        if status == StatusCode::BAD_REQUEST {
            assert_eq!(body["detail"]["param"], "url", "错误应指向 url 字段: {body}");
        }
    }

    clean(&pool).await;
}

static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

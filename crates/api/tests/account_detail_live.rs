//! `GET /api/accounts/{id}` 的真实链路测试。
//!
//! 需要 `.env` 里的 `DATABASE_URL`，缺则自动跳过。
//!
//! # 数据安全
//!
//! token 前缀按用例名分开，只删自己的行 —— 三条用例并行跑也不会互相清理。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use gpt2apirs_api::{AppState, build_router};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod support;

// 每个用例独占一个前缀：setup 会先清理自己前缀下的残留，
// 共用前缀时并行跑会把别的用例刚建的账号删掉（实测报 404 而非 200）。
const PREFIX_DETAIL: &str = "gpt2apirs-detail-one-";
const PREFIX_MISSING: &str = "gpt2apirs-detail-missing-";
const PREFIX_SCOPE: &str = "gpt2apirs-detail-scope-";

/// 登录口令：与管理面其它用例一致，从环境变量取。
fn admin_key() -> String {
    std::env::var("ADMIN_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("AUTH_KEY").ok())
        .unwrap_or_default()
}

async fn get(state: &Arc<AppState>, path: &str) -> (StatusCode, Value) {
    let response = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {}", admin_key()))
                .body(Body::empty())
                .expect("构造请求"),
        )
        .await
        .expect("请求应成功");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读 body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn make_account(pool: &PgPool, token: &str, email: &str) -> Uuid {
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let new = gpt2apirs_account::repo::NewAccount {
        access_token: token.to_string(),
        email: Some(email.to_string()),
        quota: 7,
        quota_confirmed: true,
        ..Default::default()
    };
    repo.upsert(&new).await.expect("建账号").account().id
}

/// 详情返回的字段必须与入库的一致，且结构与列表项相同。
#[tokio::test]
async fn detail_returns_the_account() {
    let Some((state, pool)) = support::setup_with(PREFIX_DETAIL).await else { return };
    let token = format!("{PREFIX_DETAIL}detail");
    let id = make_account(&pool, &token, "detail@test.local").await;

    let (status, body) = get(&state, &format!("/api/accounts/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["id"], id.to_string(), "{body}");
    assert_eq!(body["email"], "detail@test.local", "{body}");
    assert_eq!(body["quota"], 7, "{body}");
    assert_eq!(body["status_label"], "正常", "{body}");
    assert!(body["credential"].is_object(), "应有凭证视图：{body}");
    assert!(
        body.get("access_token").is_none(),
        "详情不能把 access_token 明文泄出去：{body}"
    );

    support::cleanup_with(&pool, PREFIX_DETAIL).await;
}

/// 不存在的 id → 404，而不是空对象或 500。
#[tokio::test]
async fn missing_account_is_404() {
    let Some((state, pool)) = support::setup_with(PREFIX_MISSING).await else { return };
    let missing = Uuid::new_v4();

    let (status, body) = get(&state, &format!("/api/accounts/{missing}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["detail"]["message"].as_str())
        .unwrap_or("");
    assert!(message.contains("账号不存在"), "{body}");
    // 404 不该报成 bad_request —— 客户端靠 code 分支
    assert_eq!(body["error"]["code"], "account_not_found", "{body}");

    support::cleanup_with(&pool, PREFIX_MISSING).await;
}

/// 取的是路径里那个账号，不是「随便一个」。
#[tokio::test]
async fn detail_is_scoped_to_the_path_id() {
    let Some((state, pool)) = support::setup_with(PREFIX_SCOPE).await else { return };
    let first = make_account(&pool, &format!("{PREFIX_SCOPE}a"), "a@test.local").await;
    let second = make_account(&pool, &format!("{PREFIX_SCOPE}b"), "b@test.local").await;

    let (status, body) = get(&state, &format!("/api/accounts/{second}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], second.to_string(), "{body}");
    assert_eq!(body["email"], "b@test.local", "{body}");
    assert_ne!(body["id"], first.to_string(), "{body}");

    support::cleanup_with(&pool, PREFIX_SCOPE).await;
}

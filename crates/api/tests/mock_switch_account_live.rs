//! 换号重试的端到端验证：第一个账号 401，第二个账号必须被真的用上。
//!
//! # 为什么需要
//!
//! 账号池的价值就是「这个号挂了换下一个」。以前这条只有分类单测，
//! 「真的换成功了一次」没有实证 —— 需要耗尽某个真号的配额才能人工复现。
//! 现在用 mock 上游的前 N 次失败故障，可以确定性复现。
//!
//! # 账号必须带 JWT 形状的 token
//!
//! 池子的候选筛选含「凭证准入」：解不出 `exp` 的 token 一律判不可用并跳过。
//! 所以这里造的是「有未来 exp 的假 JWT」，否则账号根本进不了候选，
//! 测试会误以为换号失败。
//!
//! # 条件
//!
//! 需要 `--features tls` 与 `DATABASE_URL`（账号池读库），缺则跳过。
//!
//! **注意**：池子里如果有别的可用账号，它可能被选中当「第一个」，
//! 因此断言写成「恰好一个账号的失败计数增加」而不是绑定具体账号；
//! 清理按邮箱前缀做 —— 造的是假 JWT，token 加前缀会解不出 exp。

#![cfg(feature = "tls")]

mod support;

use axum::http::StatusCode;
use base64::Engine as _;
use gpt2apirs_testkit::{Fault, inject};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const PREFIX: &str = "gpt2apirs-switch-test-";

/// 造一个能被凭证投影认成「有效」的假 JWT（不验签，只要求能解出未来的 exp）。
fn fake_jwt(offset_secs: i64) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let exp = chrono::Utc::now().timestamp() + offset_secs;
    let payload = engine.encode(format!(r#"{{"exp":{exp},"iat":{}}}"#, exp - 3600));
    format!("{header}.{payload}.sig")
}

/// 建 `count` 个可用账号，返回它们的 id。
async fn seed_accounts(pool: &PgPool, count: usize) -> Vec<Uuid> {
    let repo = gpt2apirs_account::AccountRepo::new(pool.clone());
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let new = gpt2apirs_account::repo::NewAccount {
            access_token: fake_jwt(3600),
            email: Some(format!("{PREFIX}{index}@test.local")),
            quota: 1,
            quota_confirmed: true,
            ..Default::default()
        };
        ids.push(repo.upsert(&new).await.expect("建账号").account().id);
    }
    ids
}

/// 全库账号的失败计数快照（用来断言「只有一个号被记账」）。
async fn fail_counts(pool: &PgPool) -> Vec<(Uuid, i64)> {
    sqlx::query_as("select id, fail_count from accounts")
        .fetch_all(pool)
        .await
        .expect("读失败计数")
}

fn body() -> Value {
    json!({
        "model": "gpt-5-5",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "说点什么"}]
    })
}

/// 核心：第一个账号 401 之后，请求必须由另一个账号完成，而不是直接失败。
#[tokio::test]
async fn text_request_survives_one_bad_account() {
    let mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = support::setup_with(PREFIX).await else { return };

    // 至少两个可用候选：一个吃 401，另一个接手
    seed_accounts(&pool, 2).await;
    state.pool.reload().await.ok();
    let before = fail_counts(&pool).await;

    let _guard = inject(Fault::FailFirstConversations {
        times: 1,
        status: 401,
    })
    .await;

    let (status, raw) = support::send(&state, "/v1/messages", body()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "第一个账号 401 后应换号重试，实际返回：{raw}"
    );

    // 两次会话请求：第一次失败、第二次成功
    let conversation_hits = mock
        .hits()
        .iter()
        .filter(|hit| hit.as_str() == "/backend-api/f/conversation")
        .count();
    assert!(
        conversation_hits >= 2,
        "应至少尝试两次上游，实际 {conversation_hits} 次：{:?}",
        mock.hits()
    );

    // 失败计数必须只落在「被换掉」的那一个号上
    let after = fail_counts(&pool).await;
    let mut increased = Vec::new();
    for (id, count) in &after {
        let previous = before
            .iter()
            .find(|(other, _)| other == id)
            .map(|(_, value)| *value)
            .unwrap_or(0);
        if *count > previous {
            increased.push(*id);
        }
    }
    assert_eq!(
        increased.len(),
        1,
        "应恰好一个账号记到失败，实际 {increased:?}"
    );

    // token 是假 JWT（加前缀会解不出 exp），所以按邮箱前缀清理
    let _ = sqlx::query("delete from accounts where email like $1")
        .bind(format!("{PREFIX}%"))
        .execute(&pool)
        .await;
    support::cleanup_with(&pool, PREFIX).await;
}

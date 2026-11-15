//! 端到端故障分类：上游异常时，对外错误码要能告诉调用方「该换号还是别重试」。
//!
//! 这条钉住的是刚修的一致性缺陷：同一类上游失败，流式与非流式曾报不同错误码
//! （非流式固定 `upstream_error`，流式按分类报）。
//!
//! 条件与数据安全见 `support`。

#![cfg(feature = "tls")]

mod support;

use axum::http::StatusCode;
use gpt2apirs_testkit::Fault;
use serde_json::{Value, json};
use support::{cleanup, send, setup};

/// 会话端点的累计命中数（mock 进程内共享，所以只能比较差值）。
fn conversation_hits(mock: &gpt2apirs_testkit::MockUpstream) -> usize {
    mock.hits()
        .iter()
        .filter(|path| path.as_str() == "/backend-api/f/conversation")
        .count()
}

fn body() -> Value {
    json!({
        "model": "text",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "随便说点什么"}]
    })
}

/// 非流式：上游 401 必须报 `account_token_invalid`，而不是笼统的 `upstream_error`。
#[tokio::test]
async fn non_stream_keeps_the_failure_classification() {
    let _mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = setup().await else { return };
    let _guard = gpt2apirs_testkit::inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 401,
        body: "{\"detail\":\"unauthorized\"}",
    })
    .await;

    let (status, raw) = send(&state, "/v1/messages", body()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{raw}");
    let payload: Value = serde_json::from_str(&raw).expect("JSON");
    assert_eq!(
        payload["error"]["code"], "account_token_invalid",
        "401 应保留分类，便于上层换号：{raw}"
    );

    cleanup(&pool).await;
}

/// 请求级失败：回 400 且不换号 —— 换号对「请求本身错了」毫无帮助。
#[tokio::test]
async fn request_level_error_returns_400_without_switching() {
    let mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = setup().await else { return };
    let _guard = gpt2apirs_testkit::inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 400,
        body: "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad params\"}}",
    })
    .await;

    let before = conversation_hits(&mock);
    let (status, raw) = send(&state, "/v1/messages", body()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "请求级失败应回 400：{raw}");
    let payload: Value = serde_json::from_str(&raw).expect("JSON");
    assert_eq!(
        payload["error"]["code"], "upstream_invalid_request",
        "应保留请求级分类：{raw}"
    );
    assert_eq!(
        payload["error"]["message"].as_str().unwrap_or("").is_empty(),
        false,
        "错误信息不该为空：{raw}"
    );

    let hits = conversation_hits(&mock) - before;
    assert_eq!(hits, 1, "请求级失败不该换号重试，本次实际请求 {hits} 次");

    cleanup(&pool).await;
}

/// 流式：同一类失败必须报同一个错误码（两条路径的口径要一致）。
#[tokio::test]
async fn stream_reports_the_same_code_as_non_stream() {
    let _mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = setup().await else { return };
    let _guard = gpt2apirs_testkit::inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 401,
        body: "{\"detail\":\"unauthorized\"}",
    })
    .await;

    let mut streaming = body();
    streaming["stream"] = json!(true);
    let (status, raw) = send(&state, "/v1/messages", streaming).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{raw}");
    let payload: Value = serde_json::from_str(&raw).expect("JSON");
    assert_eq!(payload["error"]["code"], "account_token_invalid", "{raw}");

    cleanup(&pool).await;
}

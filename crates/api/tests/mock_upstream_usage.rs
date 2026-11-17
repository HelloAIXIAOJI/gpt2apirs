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
//! 条件与数据安全见 `support`。

#![cfg(feature = "tls")]

mod support;

use axum::http::StatusCode;
use serde_json::{Value, json};
use support::{cleanup, send, setup};

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
        "model": "text",
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

    cleanup(&pool).await;
}

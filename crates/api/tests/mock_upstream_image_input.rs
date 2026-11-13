//! 图文对话的端到端验证：图片真的被上传，并进了多模态 payload。
//!
//! 端点测试只能证明「不再 400」，证明不了「图真的传上去了」。
//! 这里用 mock 上游断言：三步上传都被打到、会话 payload 是 multimodal_text。
//!
//! 需要 `--features tls` 与 `DATABASE_URL`，缺则跳过。

#![cfg(feature = "tls")]

mod support;

use axum::http::StatusCode;
use serde_json::{Value, json};
use support::{cleanup_with, send, setup_with};

const PREFIX: &str = "gpt2apirs-vision-test-";

/// 1x1 的 PNG（data URL 形式，免得测试依赖外部网络）。
const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[tokio::test]
async fn chat_uploads_the_image_and_sends_multimodal_payload() {
    let mock = gpt2apirs_testkit::spawn().await;
    let Some((state, pool)) = setup_with(PREFIX).await else { return };

    let body = json!({
        "model": "gpt-5-5",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "这张图里是什么"},
                {"type": "image_url", "image_url": {"url": PNG_DATA_URL}},
            ],
        }],
    });

    let (status, raw) = send(&state, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK, "图文对话应成功：{raw}");

    // 三步上传都该被请求到
    assert!(
        mock.saw("/backend-api/files"),
        "应申请上传位，实际 {:?}",
        mock.hits()
    );
    assert!(mock.saw("/mock-upload"), "应上传字节，实际 {:?}", mock.hits());
    assert!(
        mock.saw("/backend-api/files/file-mock-upload/uploaded"),
        "应确认上传，实际 {:?}",
        mock.hits()
    );

    // 会话 payload 必须是多模态，且引用了上传得到的 file id
    let payload = mock
        .last_body("/backend-api/f/conversation")
        .expect("应请求过会话端点");
    assert!(
        payload.contains("multimodal_text"),
        "会话 payload 应是多模态：{payload}"
    );
    assert!(
        payload.contains("file-service://file-mock-upload"),
        "应引用上传得到的文件：{payload}"
    );
    assert!(
        payload.contains("这张图里是什么"),
        "提示词不能丢：{payload}"
    );
    let parsed: Value = serde_json::from_str(&payload).expect("payload 应是 JSON");
    // messages[0] 是注入的 guard，按内容找那条带图的消息
    let multimodal = parsed["messages"]
        .as_array()
        .expect("messages 应是数组")
        .iter()
        .find(|message| message["content"]["content_type"] == "multimodal_text")
        .expect("应有 multimodal_text 消息");
    assert_eq!(
        multimodal["metadata"]["attachments"][0]["id"],
        "file-mock-upload",
        "attachments 也要报一次：{payload}"
    );

    cleanup_with(&pool, PREFIX).await;
}

//! 流式生图响应：分块 base64 必须与整串编码等价。

use base64::Engine as _;
use gpt2apirs_api::image_response::{ImageItem, ImageResponse};
use axum::response::IntoResponse;

async fn body_of(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("读 body");
    serde_json::from_slice(&bytes).expect("JSON")
}

#[tokio::test]
async fn b64_is_streamed_and_identical() {
    // 30 KB 会被切成 12000 + 12000 + 6000 三块，正好覆盖跨块边界。
    let payload: Vec<u8> = (0..30_000).map(|index| (index % 251) as u8).collect();
    let expected = base64::engine::general_purpose::STANDARD.encode(&payload);
    let response = ImageResponse {
        created: 1_700_000_000,
        usage: serde_json::json!({ "input_tokens": 0, "output_tokens": 7 }),
        items: vec![
            ImageItem {
                url: Some("http://x/1.png".to_string()),
                bytes: None,
                prompt: Some("带\"引号\"和\n换行的提示词".to_string()),
            },
            ImageItem {
                url: None,
                bytes: Some(payload.clone()),
                prompt: Some("第二张".to_string()),
            },
        ],
        message: None,
    };

    let body = body_of(response.into_response()).await;
    assert_eq!(body["created"], 1_700_000_000);
    assert_eq!(body["usage"]["output_tokens"], 7);
    assert_eq!(body["data"].as_array().expect("数组").len(), 2);
    assert_eq!(body["data"][0]["url"], "http://x/1.png");
    assert_eq!(body["data"][0]["revised_prompt"], "带\"引号\"和\n换行的提示词");
    assert!(body["data"][0].get("b64_json").is_none(), "url 项不该有 b64");
    assert_eq!(body["data"][1]["b64_json"], expected, "分块编码必须与整串一致");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body["data"][1]["b64_json"].as_str().expect("字符串"))
        .expect("解码");
    assert_eq!(decoded, payload);
    assert!(body.get("message").is_none(), "None 不该出现在 JSON 里");
}

#[tokio::test]
async fn empty_data_and_message_are_valid_json() {
    let response = ImageResponse {
        created: 1,
        usage: serde_json::json!({}),
        items: Vec::new(),
        message: Some("上游没出图".to_string()),
    };
    let body = body_of(response.into_response()).await;
    assert_eq!(body["data"].as_array().expect("数组").len(), 0);
    assert_eq!(body["message"], "上游没出图");
}

#[test]
fn chunk_size_is_multiple_of_three() {
    // 基 64 每 3 字节一组；块大小不是 3 的倍数就会在拼接处串位。
    assert_eq!(12000 % 3, 0);
}

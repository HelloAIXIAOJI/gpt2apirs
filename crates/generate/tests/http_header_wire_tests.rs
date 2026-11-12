//! 用本地 TCP 服务器抓**原始请求字节**，验证：
//!
//! 1. Content-Type 只出现一次
//! 2. 请求体是合法 JSON 字典
//!
//! 这是对「重复 Content-Type 导致上游 400」那个 bug 的直接回归证明。

#![cfg(feature = "tls")]

use gpt2apirs_http::{Client, ClientOptions};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 起一个只处理一次请求的服务器，返回 (地址, 接收原始请求的 JoinHandle)。
async fn one_shot_server() -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定失败");
    let addr = listener.local_addr().expect("取地址失败");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept 失败");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];

        // 读到头部结束
        loop {
            let n = socket.read(&mut chunk).await.expect("读失败");
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&buffer) {
                // 解析 Content-Length，继续读到 body 完整
                let head = String::from_utf8_lossy(&buffer[..pos]).to_string();
                let want: usize = head
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .and_then(|rest| rest.trim().parse().ok())
                    })
                    .unwrap_or(0);
                while buffer.len() < pos + 4 + want {
                    let n = socket.read(&mut chunk).await.expect("读 body 失败");
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                }
                break;
            }
        }

        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await;
        let _ = socket.flush().await;
        String::from_utf8_lossy(&buffer).to_string()
    });
    (format!("http://{addr}/probe"), handle)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn count_content_type(raw: &str) -> usize {
    let head = raw.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .filter(|line| line.to_ascii_lowercase().starts_with("content-type:"))
        .count()
}

fn body_of(raw: &str) -> &str {
    raw.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or("")
}

#[tokio::test]
async fn post_json_sends_single_content_type_and_dict_body() {
    let (url, handle) = one_shot_server().await;

    let client = Client::new(ClientOptions::default()).expect("构建客户端");
    let headers = vec![
        ("Accept", "*/*".to_string()),
        // 故意带一个自定义头，确认头部生效
        ("X-Test", "yes".to_string()),
    ];
    let response = client
        .post_json(&url, &headers, &json!({ "hello": "world", "n": 1 }), None)
        .await
        .expect("请求失败");

    assert_eq!(response.status, 200);

    let raw = handle.await.expect("服务器 task 失败");

    // 核心断言 1：Content-Type 只能有一个
    assert_eq!(
        count_content_type(&raw),
        1,
        "Content-Type 应恰好出现一次，实际原始请求：\n{raw}"
    );

    // 核心断言 2：body 必须是 JSON 字典
    let body = body_of(&raw).trim();
    let parsed: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|err| panic!("请求体不是合法 JSON: {err}\nbody={body:?}\nraw={raw}"));
    assert!(parsed.is_object(), "请求体应是 JSON 对象，实际: {parsed}");
    assert_eq!(parsed["hello"], json!("world"));
    assert_eq!(parsed["n"], json!(1));

    // 自定义头确实发出去了
    assert!(
        raw.to_ascii_lowercase().contains("x-test: yes"),
        "自定义头未发出：\n{raw}"
    );
}

#[tokio::test]
async fn get_text_sends_no_content_type() {
    let (url, handle) = one_shot_server().await;
    let client = Client::new(ClientOptions::default()).expect("构建客户端");

    let headers = vec![("Accept", "application/json".to_string())];
    let response = client.get_text(&url, &headers, None).await.expect("请求失败");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "{}");

    let raw = handle.await.expect("服务器 task 失败");
    assert_eq!(
        count_content_type(&raw),
        0,
        "GET 不应带 Content-Type：\n{raw}"
    );
}

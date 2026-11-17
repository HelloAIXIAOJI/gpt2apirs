//! 守护「mock 上游可以跑在明文 http://127.0.0.1」这个前提。
//!
//! 带 TLS 指纹的客户端未必要走 TLS：如果哪天 wreq 拒绝明文 scheme，
//! 离线 mock 上游就得改成自签证书方案 —— 那会连带改测试基础设施，
//! 所以在这里钉一条最小往返测试，失败即说明 mock 方案要重做。
//!
//! 需要 `--features tls`（默认特性下本文件不编译）。

#![cfg(feature = "tls")]

use std::time::Duration;

use gpt2apirs_http::{Client, ClientOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn wreq_can_hit_plain_local_http() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let body = "{\"ok\":true}";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    let client = Client::new(ClientOptions::default()).expect("client");
    let url = format!("http://{addr}/probe");
    let got = client
        .get_text(&url, &[("accept", "*/*".to_string())], Some(Duration::from_secs(5)))
        .await;

    match got {
        Ok(response) => {
            println!("SPIKE_OK status={} body={}", response.status, response.body);
            assert_eq!(response.status, 200);
        }
        Err(error) => panic!("SPIKE_FAIL: 明文 http 打不通：{error:#}"),
    }
}

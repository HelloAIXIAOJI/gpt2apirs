//! 批量打包下载：tar.gz 流必须可解、条目与内容对得上。

use std::io::Read;
use std::path::PathBuf;

use axum::response::IntoResponse;

#[tokio::test]
async fn streams_valid_tar_gz() {
    let dir = std::env::temp_dir().join(format!("gpt2apirs-zip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建目录");
    let first: Vec<u8> = (0..1024).map(|index| (index % 251) as u8).collect();
    let second = b"hello".to_vec();
    std::fs::write(dir.join("a.png"), &first).expect("写 a");
    std::fs::write(dir.join("b.jpg"), &second).expect("写 b");

    let items = vec![
        ("images/2026/11/17/a.png".to_string(), PathBuf::from(dir.join("a.png"))),
        ("images/2026/11/17/b.jpg".to_string(), PathBuf::from(dir.join("b.jpg"))),
        (
            "images/2026/11/17/missing.png".to_string(),
            PathBuf::from(dir.join("missing.png")),
        ),
    ];
    let response = gpt2apirs_api::images_download::tar_gz_stream("x.tar.gz".to_string(), items);
    assert_eq!(
        response.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/gzip")
    );
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("读 body");
    assert!(!bytes.is_empty(), "应有内容");

    // 解包：缺失文件应被跳过，两个存在的内容要逐字节一致。
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive.entries().expect("条目") {
        let mut entry = entry.expect("条目");
        let path = entry.path().expect("路径").to_string_lossy().to_string();
        let mut buffer = Vec::new();
        entry.read_to_end(&mut buffer).expect("读条目");
        found.push((path, buffer));
    }
    assert_eq!(found.len(), 2, "缺失的文件应被跳过：{found:?}");
    assert_eq!(found[0].0, "images/2026/11/17/a.png");
    assert_eq!(found[0].1, first);
    assert_eq!(found[1].1, second);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn empty_list_yields_empty_body() {
    // 没有图片时也要回一个合法响应（空包），不能让请求挂着。
    let response = gpt2apirs_api::images_download::tar_gz_stream("x.tar.gz".to_string(), Vec::new());
    assert_eq!(response.status(), 200);
    let _ = response.into_response();
}

//! 图片七步流水线的离线测试：上游是本地 mock 服务。
//!
//! # 覆盖
//!
//! bootstrap → sentinel（prepare/finalize）→ conversation/prepare（conduit）→
//! 会话 SSE（带 file-service 指针）→ 解析下载地址 → 下载字节 → 落盘。
//!
//! # 怎么跑
//!
//! ```text
//! powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_upstream_image
//! ```

#![cfg(feature = "tls")]

use std::time::Duration;

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::ImageRequest;
use gpt2apirs_generate::generate_images;
use gpt2apirs_http::{Client, ClientOptions};
use gpt2apirs_testkit::{IMAGE_CONVERSATION_ID, IMAGE_FILE_ID, spawn};

/// 落盘目录指到临时目录，别污染仓库的 data/。
fn point_data_dir() {
    if std::env::var("DATA_DIR").is_ok() {
        return;
    }
    let dir = std::env::temp_dir().join("gpt2apirs-testkit-data");
    std::fs::create_dir_all(&dir).expect("建临时数据目录");
    // SAFETY 说明：测试进程内只设一次，且在任何落盘之前
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("DATA_DIR", &dir);
    }
}

#[tokio::test]
async fn image_pipeline_runs_against_local_mock() {
    point_data_dir();
    let mock = spawn().await;

    let client = Client::new(ClientOptions::default()).expect("客户端");
    let request = ImageRequest::new("画一只猫");
    let deadline = Deadline::after(Duration::from_secs(30));

    let (result, diag) = generate_images(&client, "mock-token", &request, deadline)
        .await
        .expect("mock 上游下生图链路应成功");

    assert_eq!(result.conversation_id, IMAGE_CONVERSATION_ID);
    assert!(
        result.file_ids.iter().any(|id| id == IMAGE_FILE_ID),
        "应从 SSE 里解析出文件 id，实际 {:?}",
        result.file_ids
    );
    assert_eq!(result.images.len(), 1, "应下载并落盘一张图");

    let image = &result.images[0];
    assert!(!image.bytes.is_empty(), "落盘字节数应大于 0：{image:?}");
    assert!(image.width > 0 && image.height > 0, "应读到图片尺寸：{image:?}");
    assert_eq!(image.mime, "image/png", "{image:?}");
    // rel_path 是相对**数据目录**的（形如 images/2026/11/13/xxx.png）
    let saved = gpt2apirs_core::paths::data_dir()
        .expect("数据目录")
        .join(&image.rel_path);
    assert!(saved.exists(), "文件应真的存在：{}", saved.display());

    // 七个阶段都得真的走到
    assert_eq!(diag.bootstrap_status, 200);
    assert_eq!(diag.prepare_status, 200);
    assert_eq!(diag.finalize_status, 200);
    assert_eq!(diag.conv_prepare_status, 200);
    assert_eq!(diag.sse_status, 200);

    assert!(mock.saw("/backend-api/f/conversation/prepare"), "应请求会话准备");
    assert!(
        mock.hits().iter().any(|hit| hit.starts_with("/backend-api/files/")),
        "应解析下载地址，实际 {:?}",
        mock.hits()
    );
    assert!(
        mock.saw("/mock-asset.png"),
        "应真的下载图片字节，实际 {:?}",
        mock.hits()
    );

    // 收尾：删掉落盘文件
    let _ = std::fs::remove_file(&saved);
}

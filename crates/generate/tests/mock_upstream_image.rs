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
    // mock 是进程内全局的：持锁把所有用例串起来，免得互相污染
    let _serial = gpt2apirs_testkit::inject(gpt2apirs_testkit::Fault::Healthy).await;

    let client = Client::new(ClientOptions::default()).expect("客户端");
    let request = ImageRequest::new("画一只猫")
        .with_size(Some("1024x1024".to_string()))
        .with_quality(Some("high".to_string()));
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

    // 尺寸不是上游参数，只能体现在提示词里：断言它真的到了 payload 里
    let payload = mock
        .last_body("/backend-api/f/conversation")
        .expect("应请求过会话端点");
    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("payload 应是 JSON");
    let prompt_text = parsed["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"]["parts"].as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    assert!(
        prompt_text.contains("输出图片尺寸为 1024x1024。"),
        "提示词里应带上尺寸指令，实际：{prompt_text}"
    );
    assert!(
        prompt_text.contains("输出图片质量为 high。"),
        "提示词里应带上质量指令，实际：{prompt_text}"
    );

    let image = &result.images[0];
    // 默认不保留字节（url 模式白占内存），但 size 必须准确、文件必须真的写下去。
    assert!(image.size > 0, "落盘字节数应大于 0：{image:?}");
    assert!(
        image.bytes.is_empty(),
        "没声明 keep_bytes 时不该把字节留在内存里：{image:?}"
    );
    assert!(image.width > 0 && image.height > 0, "应读到图片尺寸：{image:?}");
    assert_eq!(image.mime, "image/png", "{image:?}");
    // rel_path 是相对**数据目录**的（形如 images/2026/11/13/xxx.png）
    let saved = gpt2apirs_core::paths::data_dir()
        .expect("数据目录")
        .join(&image.rel_path);
    assert!(saved.exists(), "文件应真的存在：{}", saved.display());
    let on_disk = std::fs::metadata(&saved).expect("读元数据").len();
    assert_eq!(on_disk as usize, image.size, "落盘大小应与 size 一致");

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

/// 上游只回文本、不出图：必须当场按请求级失败返回，不许轮询、不许换号。
///
/// 这条是「502 请求总时限已耗尽（阶段 bootstrap）」那个线上问题的回归测试：
/// 以前文本被丢掉，没图就进 60 秒轮询，然后判成可换号，最后拿空预算撞 bootstrap。
#[tokio::test]
async fn text_reply_without_image_fails_fast() {
    point_data_dir();
    let mock = spawn().await;
    let _fault = gpt2apirs_testkit::inject(gpt2apirs_testkit::Fault::ImageTextOnly).await;

    let client = Client::new(ClientOptions::default()).expect("客户端");
    let request = ImageRequest::new("画一只猫");
    // 给足预算：如果能通过，说明不是被时限拦下的
    let deadline = Deadline::after(Duration::from_secs(60));
    // mock 的命中记录是进程级累加的，只看本次请求产生的那一段
    let baseline = mock.hits().len();

    let error = generate_images(&client, "mock-token", &request, deadline)
        .await
        .expect_err("只有文本没有图时不该算成功");

    assert_eq!(
        error.kind,
        gpt2apirs_generate::error::FailureKind::TextReply,
        "应判成「上游只回文本」：{error:?}"
    );
    assert!(
        error.message.contains(gpt2apirs_testkit::IMAGE_TEXT_REPLY),
        "上游那段话要原样带出去，实际：{}",
        error.message
    );
    assert!(!error.kind.switch_account(), "文本回复不该换号");

    // 关键：不能进兜底轮询。允许的只有「判失败前那一次确认」
    let mine: Vec<String> = mock.hits()[baseline..].to_vec();
    let detail_hits = mine
        .iter()
        .filter(|hit| hit.starts_with("/backend-api/conversation/"))
        .count();
    assert!(
        detail_hits <= 1,
        "最多查一次会话详情确认没图，不该轮询：{detail_hits} 次，命中 {mine:?}"
    );
}

/// SSE 里没有指针、但会话详情里有图：必须去捞回来，不能判成「只回文本」。
///
/// 出图之后助手往往会补一段说明文字，只看 SSE 会把「图在会话里」误判成拒绝。
#[tokio::test]
async fn image_found_in_conversation_detail_is_not_a_text_reply() {
    point_data_dir();
    let mock = spawn().await;
    let _fault = gpt2apirs_testkit::inject(
        gpt2apirs_testkit::Fault::ImageTextOnlyWithDetailImage,
    )
    .await;

    let client = Client::new(ClientOptions::default()).expect("客户端");
    let request = ImageRequest::new("画一只猫");
    let deadline = Deadline::after(Duration::from_secs(60));
    let baseline = mock.hits().len();

    let (result, _diag) = generate_images(&client, "mock-token", &request, deadline)
        .await
        .expect("图在会话详情里，应被捞回来");

    assert_eq!(result.images.len(), 1, "应下载并落盘一张图：{result:?}");
    let mine: Vec<String> = mock.hits()[baseline..].to_vec();
    assert!(
        mine.iter().any(|hit| hit.starts_with("/backend-api/conversation/")),
        "应查过会话详情把图捞回来，实际 {mine:?}"
    );
}

//! 流式测试的共享支撑（仅测试构建可见）。

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use gpt2apirs_generate::error::GenerateError;
use gpt2apirs_generate::text::StreamSnapshot;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::routes::text_common::StreamHandle;

/// 一条增量项。
pub type DeltaItem = std::result::Result<String, GenerateError>;

/// 建一个可手动喂数据的 {@link StreamHandle}。
///
/// 只要 `sender` 还活着，源流就不算结束 —— 这正是检验「边收边吐」的手段：
/// 缓冲实现必须等源流结束才能产出，因此永远拿不到「源未结束就来 delta」。
pub fn channel_handle() -> (mpsc::Sender<DeltaItem>, StreamHandle) {
    let (sender, receiver) = mpsc::channel::<DeltaItem>(8);
    let deltas = futures_util::stream::unfold(receiver, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed();
    let handle = StreamHandle {
        deltas,
        snapshot: Arc::new(Mutex::new(StreamSnapshot::default())),
        model: gpt2apirs_generate::models::DEFAULT_TEXT_MODEL.to_string(),
        billing_messages: vec![("user".to_string(), "你好".to_string())],
        account_id: Uuid::nil(),
    };
    (sender, handle)
}

/// 读完整个响应体。
pub async fn collect_body(response: axum::response::Response) -> String {
    let mut body = response.into_body().into_data_stream();
    let mut buffer = String::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.expect("响应块");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
    buffer
}

/// 断言某个子串出现的位置早于另一个子串。
#[track_caller]
pub fn assert_before(haystack: &str, first: &str, second: &str) {
    let a = haystack.find(first).unwrap_or_else(|| panic!("缺少 {first}"));
    let b = haystack.find(second).unwrap_or_else(|| panic!("缺少 {second}"));
    assert!(a < b, "{first} 应早于 {second}");
}

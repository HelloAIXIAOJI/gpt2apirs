//! 文本生成链路的离线测试：上游是本地的 mock 服务。
//!
//! # 为什么需要
//!
//! 之前所有生成链路测试都要求真账号 + 真网络，于是「改完绿了、上线错了」
//! （流式 usage 把内部 guard 算进账单，575 条测试全绿却漏掉）没法防。
//! 这里让整条链路（bootstrap → sentinel → SSE）真跑，但不出环回地址。
//!
//! # 怎么跑
//!
//! 需要 `--features tls`：默认特性下 `gpt2apirs-http::Client` 是存根，发不出请求。
//!
//! ```text
//! powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_upstream
//! ```

#![cfg(feature = "tls")]

use std::time::Duration;

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::models;
use gpt2apirs_generate::text::{TextRequest, generate_text};
use gpt2apirs_http::{Client, ClientOptions};
use gpt2apirs_testkit::{CONVERSATION_ID, REPLY_TEXT, spawn};

#[tokio::test]
async fn text_generation_runs_against_local_mock() {
    let mock = spawn().await;
    assert_eq!(
        gpt2apirs_protocol::endpoints::base_url(),
        mock.base(),
        "上游站点应已被指向本地 mock"
    );

    let client = Client::new(ClientOptions::default()).expect("客户端");
    let mut request = TextRequest::new("说点什么".to_string());
    request.model = models::DEFAULT_TEXT_MODEL.to_string();
    request.upstream_model = models::upstream_model_for(models::DEFAULT_TEXT_MODEL).to_string();

    let deadline = Deadline::after(Duration::from_secs(30));
    let (result, diag) = generate_text(&client, "mock-token", &request, deadline)
        .await
        .expect("mock 上游下文本链路应成功");

    assert_eq!(result.text, REPLY_TEXT, "SSE 补丁应被拼回正文");
    assert_eq!(result.conversation_id, CONVERSATION_ID);
    assert!(!result.deltas.is_empty(), "增量序列不该为空");

    // 四个阶段都必须真的打到 mock 上：任何一格是 0 都说明链路被短路了
    assert_eq!(diag.bootstrap_status, 200);
    assert_eq!(diag.prepare_status, 200);
    assert_eq!(diag.finalize_status, 200);
    assert_eq!(diag.sse_status, 200);
    assert!(diag.sse_bytes > 0, "SSE 字节数应被记录");

    assert!(mock.saw("/"), "应请求过首页");
    assert!(
        mock.saw("/backend-api/f/conversation"),
        "应请求过会话端点，实际收到 {:?}",
        mock.hits()
    );
}

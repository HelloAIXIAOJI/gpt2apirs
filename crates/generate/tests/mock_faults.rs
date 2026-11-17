//! 故障路径的确定性复现：上游异常时**分类对不对**。
//!
//! # 为什么需要
//!
//! 换号重试只在「换号有意义」的分类上触发：401 该换号，内容策略不该换号。
//! 这些分支以前只有单元测试喂状态码，没跑过真实链路；
//! 而「真的会在链路上抛出这个分类」只有端到端能证明。
//!
//! # 怎么跑
//!
//! ```text
//! powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_faults
//! ```
//!
//! 故障是进程级全局状态，每个用例都用 `inject()` 拿串行令牌，互不干扰。

#![cfg(feature = "tls")]

use std::time::Duration;

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::error::{FailureKind, GenerateError};
use gpt2apirs_generate::models;
use gpt2apirs_generate::text::{TextRequest, generate_text};
use gpt2apirs_http::{Client, ClientOptions};
use gpt2apirs_testkit::{Fault, inject, spawn};

fn request() -> TextRequest {
    let mut request = TextRequest::new("随便说点什么".to_string());
    request.model = models::DEFAULT_TEXT_MODEL.to_string();
    request.upstream_model = models::upstream_model_for(models::DEFAULT_TEXT_MODEL).to_string();
    request
}

/// 取出带分类的生成错误：链路里的错误都是 `GenerateError`，别的类型说明分类丢了。
#[track_caller]
fn expect_generate_error(error: &anyhow::Error) -> &GenerateError {
    error
        .downcast_ref::<GenerateError>()
        .unwrap_or_else(|| panic!("应是带分类的生成错误，实际：{error:#}"))
}

async fn call() -> Result<String, anyhow::Error> {
    let client = Client::new(ClientOptions::default()).expect("客户端");
    let deadline = Deadline::after(Duration::from_secs(20));
    let (result, _) = generate_text(&client, "mock-token", &request(), deadline).await?;
    Ok(result.text)
}

/// 401 = 账号 token 失效：必须可换号，且要把账号算在头上。
#[tokio::test]
async fn unauthorized_blames_the_account_and_can_switch() {
    let _mock = spawn().await;
    let _guard = inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 401,
        body: "{\"detail\":\"unauthorized\"}",
    })
    .await;

    let error = call().await.expect_err("401 必须失败");
    let failure = expect_generate_error(&error);
    assert_eq!(failure.kind, FailureKind::TokenInvalid, "{}", failure.message);
    assert!(failure.kind.switch_account(), "401 应触发换号");
    assert!(failure.kind.blames_account(), "401 应把账号记为可疑");
    assert!(failure.message.contains("401"), "错误信息应带状态码：{}", failure.message);
}

/// 429 = 限流：必须可换号。
///
/// 注意 `blames_account()` 对 429 也返回 true（见 error.rs 的既有取舍），
/// 这里不断言它 —— 那条判定属于账号池的失效策略，不是这条链路的契约。
#[tokio::test]
async fn rate_limit_is_classified_and_can_switch() {
    let _mock = spawn().await;
    let _guard = inject(Fault::Status {
        path_contains: "/backend-api/sentinel/chat-requirements/prepare",
        status: 429,
        body: "",
    })
    .await;

    let error = call().await.expect_err("429 必须失败");
    let failure = expect_generate_error(&error);
    assert_eq!(failure.kind, FailureKind::RateLimited, "{}", failure.message);
    assert!(failure.kind.switch_account(), "限流应换号");
    assert_ne!(
        failure.kind,
        FailureKind::Protocol,
        "限流不该被降级成协议错误"
    );
}

/// 状态码是 500，但响应体说 quota：以响应体为准判成配额耗尽。
#[tokio::test]
async fn quota_in_body_overrides_status_code() {
    let _mock = spawn().await;
    let _guard = inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 500,
        body: "{\"detail\":\"quota exceeded\"}",
    })
    .await;

    let error = call().await.expect_err("配额耗尽必须失败");
    let failure = expect_generate_error(&error);
    assert_eq!(failure.kind, FailureKind::QuotaExhausted, "{}", failure.message);
    assert!(failure.kind.switch_account(), "配额耗尽应换号");
}

/// 内容策略：换号没用，也不该把账号标失效。
#[tokio::test]
async fn content_policy_never_switches_account() {
    let _mock = spawn().await;
    let _guard = inject(Fault::Status {
        path_contains: "/backend-api/f/conversation",
        status: 400,
        body: "{\"error\":\"content_policy_violation\"}",
    })
    .await;

    let error = call().await.expect_err("内容策略必须失败");
    let failure = expect_generate_error(&error);
    assert_eq!(failure.kind, FailureKind::ContentPolicy, "{}", failure.message);
    assert!(!failure.kind.switch_account(), "内容策略换号没用");
    assert!(!failure.kind.blames_account(), "内容策略不是账号的错");
}

/// SSE 中途断流：已收到的部分不该丢，但整条调用必须报错而不是假装成功。
#[tokio::test]
async fn truncated_sse_does_not_pretend_success() {
    let _mock = spawn().await;
    // 只发到第一条 patch 的一半，正文不完整
    let _guard = inject(Fault::TruncateSse(CONVERSATION_HEAD)).await;

    let outcome = call().await;
    match outcome {
        Ok(text) => panic!("断流不该算成功，实际拿到正文：{text:?}"),
        Err(error) => {
            assert!(!error.to_string().is_empty(), "断流要给出可诊断的错误信息");
        }
    }
}

/// 断流位置：切在第一个 patch 中间。
const CONVERSATION_HEAD: usize = 120;

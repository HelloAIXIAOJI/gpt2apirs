//! 本地 mock 上游：让文本生成链路**不碰真实网络**地跑一遍。
//!
//! # 为什么需要
//!
//! 之前所有生成链路测试都要求真账号 + 真网络，于是「改完绿了、上线错了」
//! （流式 usage 把内部 guard 算进账单，575 条测试全绿却漏掉）没法防。
//! 这里用本地 HTTP 服务接住出站请求，链路、SSE 分帧、解析全部真跑。
//!
//! # 怎么跑
//!
//! 需要 `--features tls`：默认特性下 `gpt2apirs-http::Client` 是存根，发不出请求。
//!
//! ```text
//! powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_upstream
//! ```
//!
//! # 约束
//!
//! 上游站点由 `GPT2APIRS_UPSTREAM_BASE` 指向本地，而它在进程内只解析一次，
//! 所以本文件**只能有一个`测试`在跑**：全部断言写在一个用例里，
//! 多个端口会互相打架。

#![cfg(feature = "tls")]

use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use gpt2apirs_core::Deadline;
use gpt2apirs_generate::models;
use gpt2apirs_generate::text::{TextRequest, generate_text};
use gpt2apirs_http::{Client, ClientOptions};

/// mock 服务的监听地址（进程内只起一次）。
static BASE: OnceLock<String> = OnceLock::new();

/// 首页：只要有一个带 `c/<build>/_` 的 script src，就能被 parse_pow_resources 认出 data-build。
const HOME_HTML: &str = r#"<!doctype html><html><head>
<script src="/c/mockbuild123/_/pow.js"></script>
</head><body>mock</body></html>"#;

/// prepare：明确不要求 PoW / arkose / turnstile，链路直接往下走。
const PREPARE_JSON: &str = r#"{"prepare_token":"mock-prepare-token",
  "proofofwork":{"required":false},
  "arkose":{"required":false},
  "turnstile":{"required":false}}"#;

/// finalize：必须带回非空 token，否则握手就失败了。
const FINALIZE_JSON: &str = r#"{"token":"mock-requirements-token","so_token":"mock-so","expire_after":3600}"#;

/// 会话 SSE：两条 append 补丁 + 结束标记，拼出来是「本地回复」。
const CONVERSATION_SSE: &str = "data: \"v1\"\n\ndata: {\"conversation_id\":\"mock-conv-1\"}\n\ndata: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"本地\"}]}\n\ndata: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"回复\"}]}\n\ndata: [DONE]\n\n";

/// 记录 mock 收到了哪些请求，用来断言「链路真的走完了」。
#[derive(Debug, Default)]
struct Hits(std::sync::Mutex<Vec<String>>);

async fn record(
    axum::extract::State(hits): axum::extract::State<std::sync::Arc<Hits>>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    if let Ok(mut guard) = hits.0.lock() {
        guard.push(path.clone());
    }
    match path.as_str() {
        "/" => HOME_HTML.to_string().into_response(),
        p if p.ends_with("/sentinel/chat-requirements/prepare") => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            PREPARE_JSON,
        )
            .into_response(),
        p if p.ends_with("/sentinel/chat-requirements/finalize") => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            FINALIZE_JSON,
        )
            .into_response(),
        "/backend-api/f/conversation" => (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            CONVERSATION_SSE,
        )
            .into_response(),
        other => (
            axum::http::StatusCode::NOT_FOUND,
            format!("mock 未定义该路径: {other}"),
        )
            .into_response(),
    }
}

/// 起 mock 服务并把上游站点指过去（进程内只做一次）。
async fn mock_base() -> String {
    if let Some(base) = BASE.get() {
        return base.clone();
    }
    let hits = std::sync::Arc::new(Hits::default());
    let app = Router::new()
        .route("/", get(record))
        .route("/backend-api/f/conversation", post(record))
        .route("/backend-api/sentinel/chat-requirements/prepare", post(record))
        .route("/backend-api/sentinel/chat-requirements/finalize", post(record))
        .with_state(hits);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock 服务应能绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock 服务应持续运行");
    });

    let base = format!("http://{addr}");
    // 必须在任何出站请求之前设置：base_url() 在进程内只解析一次
    unsafe { std::env::set_var(gpt2apirs_protocol::endpoints::BASE_URL_ENV, &base) };
    BASE.set(base.clone()).ok();
    base
}

#[tokio::test]
async fn text_generation_runs_against_local_mock() {
    let base = mock_base().await;
    assert_eq!(
        gpt2apirs_protocol::endpoints::base_url(),
        base,
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

    assert_eq!(result.text, "本地回复", "SSE 补丁应被拼回正文");
    assert_eq!(result.conversation_id, "mock-conv-1");
    // 四个阶段都必须真的打到 mock 上：任何一格是 0 都说明链路被短路了
    assert_eq!(diag.bootstrap_status, 200);
    assert_eq!(diag.prepare_status, 200);
    assert_eq!(diag.finalize_status, 200);
    assert_eq!(diag.sse_status, 200);
    assert!(diag.sse_bytes > 0, "SSE 字节数应被记录");
    assert!(!result.deltas.is_empty(), "增量序列不该为空");
}

//! 本地 mock 上游：让出站链路跑在环回地址上，不碰真实网络。
//!
//! # 为什么单独成 crate
//!
//! 需要它的测试分属不同 crate（generate 测流水线、api 测端到端 usage 口径）。
//! 放在任一方的 tests/ 里都只能给那一方用，复制一份又会两边漂移，
//! 所以做成不发布的 dev crate。
//!
//! # 怎么接
//!
//! ```text
//! let mock = spawn().await;           // 起服务，并把上游站点指过来
//! // 之后任何走 gpt2apirs_protocol::endpoints::base_url() 的请求都打到 mock
//! assert!(mock.hits().contains(&"/backend-api/f/conversation".to_string()));
//! ```
//!
//! **进程内只能起一次**：上游站点由环境变量决定，而它在进程内只解析一次。
//! 第二次调用会直接复用第一次的地址，不会换端口。

#![deny(unsafe_code)]

use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// 首页：带 `c/<build>/_` 的 script src 才让 parse_pow_resources 认出 data-build。
const HOME_HTML: &str = r#"<!doctype html><html><head>
<script src="/c/mockbuild123/_/pow.js"></script>
</head><body>mock</body></html>"#;

/// prepare：不要求 PoW / arkose / turnstile，链路直接往下走。
const PREPARE_JSON: &str = r#"{"prepare_token":"mock-prepare-token",
  "proofofwork":{"required":false},
  "arkose":{"required":false},
  "turnstile":{"required":false}}"#;

/// finalize：必须带回非空 token，否则握手失败。
const FINALIZE_JSON: &str =
    r#"{"token":"mock-requirements-token","so_token":"mock-so","expire_after":3600}"#;

/// 会话 SSE：两条 append 补丁 + 结束标记，拼出来是「本地回复」。
pub const CONVERSATION_SSE: &str = "data: \"v1\"\n\ndata: {\"conversation_id\":\"mock-conv-1\"}\n\ndata: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"本地\"}]}\n\ndata: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"回复\"}]}\n\ndata: [DONE]\n\n";

/// mock 上游的回复正文，测试可直接断言。
pub const REPLY_TEXT: &str = "本地回复";

/// 会话 ID。
pub const CONVERSATION_ID: &str = "mock-conv-1";

/// 已启动的 mock 上游。
#[derive(Clone)]
pub struct MockUpstream {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl MockUpstream {
    /// 站点地址，例如 `http://127.0.0.1:54321`。
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 收到过的请求路径（按到达顺序）。
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// 是否收到过某条路径。
    pub fn saw(&self, path: &str) -> bool {
        self.hits().iter().any(|hit| hit == path)
    }
}

async fn handle(
    State(hits): State<Arc<Mutex<Vec<String>>>>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    if let Ok(mut guard) = hits.lock() {
        guard.push(path.clone());
    }
    match path.as_str() {
        "/" => HOME_HTML.to_string().into_response(),
        p if p.ends_with("/sentinel/chat-requirements/prepare") => json(PREPARE_JSON),
        p if p.ends_with("/sentinel/chat-requirements/finalize") => json(FINALIZE_JSON),
        "/backend-api/f/conversation" => (
            [("content-type", "text/event-stream")],
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

fn json(body: &'static str) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// 把进程里的上游站点指向本地。
///
/// 除了这个赋值本身，没有别的办法让既有调用点（不持有配置对象）改指向；
/// 赋值只做一次、且在请求发出前完成，见模块文档的进程内唯一约束。
#[allow(unsafe_code)]
fn point_upstream_at(base: &str) {
    unsafe { std::env::set_var(gpt2apirs_protocol::endpoints::BASE_URL_ENV, base) };
}

static STARTED: OnceLock<MockUpstream> = OnceLock::new();

/// 起 mock 上游，并把进程里的上游站点指过来。
///
/// 需要 tokio 运行时。进程内只真正启动一次。
pub async fn spawn() -> MockUpstream {
    if let Some(existing) = STARTED.get() {
        return existing.clone();
    }
    let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/", get(handle))
        .route("/backend-api/f/conversation", post(handle))
        .route("/backend-api/sentinel/chat-requirements/prepare", post(handle))
        .route("/backend-api/sentinel/chat-requirements/finalize", post(handle))
        .with_state(hits.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock 上游应能绑定端口");
    let addr = listener.local_addr().expect("本地地址");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock 上游应持续运行");
    });

    let base = format!("http://{addr}");
    point_upstream_at(&base);

    let mock = MockUpstream { base, hits };
    STARTED.set(mock.clone()).ok();
    mock
}

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
use axum::routing::get;

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

/// 图片链路：会话准备返回的 conduit token。
pub const CONDUIT_TOKEN: &str = "mock-conduit-token";

/// 图片链路：会话准备响应。
const PREPARE_CONVERSATION_JSON: &str =
    r#"{"conduit_token":"mock-conduit-token","status":"ok"}"#;

/// 图片链路：SSE 里要能被识别出的文件 id（`file_` + 8 个 0 + 24 位十六进制）。
pub const IMAGE_FILE_ID: &str = "file_00000000aabbccddeeff001122334455";

/// 图片链路：会话 id。
pub const IMAGE_CONVERSATION_ID: &str = "mock-image-conv";

/// 图文对话上传：文件 id 与 PUT 目标路径。
const UPLOAD_FILE_ID: &str = "file-mock-upload";
const UPLOAD_PATH: &str = "/mock-upload";

/// 图片链路：下载路径的固定后缀，用于区分「解析地址」与「取字节」。
pub const IMAGE_ASSET_PATH: &str = "/mock-asset.png";

/// mock 上游的回复正文，测试可直接断言。
pub const REPLY_TEXT: &str = "本地回复";

/// 会话 ID。
pub const CONVERSATION_ID: &str = "mock-conv-1";

/// 已启动的 mock 上游。
#[derive(Clone)]
pub struct MockUpstream {
    base: String,
    hits: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockUpstream {
    /// 站点地址，例如 `http://127.0.0.1:54321`。
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 收到过的请求路径（按到达顺序）。
    pub fn hits(&self) -> Vec<String> {
        self.hits
            .lock()
            .map(|guard| guard.iter().map(|(path, _)| path.clone()).collect())
            .unwrap_or_default()
    }

    /// 某条路径**最后一次**收到的请求体；没收到过返回 `None`。
    pub fn last_body(&self, path: &str) -> Option<String> {
        self.hits
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .iter()
                    .rev()
                    .find(|(hit, _)| hit == path)
                    .map(|(_, body)| body.clone())
            })
    }

    /// 是否收到过某条路径。
    pub fn saw(&self, path: &str) -> bool {
        self.hits().iter().any(|hit| hit == path)
    }
}

/// 图片会话 SSE：正文里带 file-service 指针 + conversation_id。
fn image_sse() -> String {
    format!(
        "data: {{\"conversation_id\":\"{IMAGE_CONVERSATION_ID}\"}}\n\n\
data: {{\"o\":\"patch\",\"v\":[{{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\
\"v\":\"{{\\\"asset_pointer\\\":\\\"file-service://{IMAGE_FILE_ID}\\\"}}\"}}]}}\n\n\
data: [DONE]\n\n"
    )
}

/// 上游「只回文本、不出图」时那段话。
///
/// 生图链路必须把它原样当成请求级失败带出去，而不是报一个空错误。
pub const IMAGE_TEXT_REPLY: &str = "抱歉，我无法生成这张图片。";

/// 图片会话 SSE：只有助手文本，没有任何图片指针。
fn image_text_only_sse() -> String {
    // 用 json! 拼：手写 JSON 要同时躲 Rust 转义和 format! 的花括号转义，太容易多一层。
    let message = serde_json::json!({
        "v": {"message": {
            "id": "mock-msg",
            "author": {"role": "assistant"},
            "content": {"content_type": "text", "parts": [IMAGE_TEXT_REPLY]},
            "recipient": "all",
            "channel": "final"
        }}
    });
    format!(
        "data: {{\"conversation_id\":\"{IMAGE_CONVERSATION_ID}\"}}\n\ndata: {message}\n\ndata: [DONE]\n\n"
    )
}

/// 会话详情：assistant 消息里带着图片指针。
fn conversation_detail_with_image() -> String {
    serde_json::json!({
        "id": IMAGE_CONVERSATION_ID,
        "mapping": {
            "n1": {"message": {
                "id": "mock-image-msg",
                "author": {"role": "assistant"},
                "content": {
                    "content_type": "multimodal_text",
                    "parts": [
                        {
                            "content_type": "image_asset_pointer",
                            "asset_pointer": format!("file-service://{IMAGE_FILE_ID}"),
                        },
                        IMAGE_TEXT_REPLY,
                    ],
                },
                "create_time": 1.0,
                "metadata": {},
            }},
        },
    })
    .to_string()
}

/// 一张 2x2 的合法 PNG，供下载步骤读取。
fn png_bytes() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]));
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("编码 PNG");
    out.into_inner()
}

async fn handle(
    State(hits): State<Arc<Mutex<Vec<(String, String)>>>>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    // 读一次 body：断言 payload 形状要靠它，上限 1 MiB 足够
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string();
    if let Ok(mut guard) = hits.lock() {
        guard.push((path.clone(), text));
    }
    let request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    // 「前 N 次失败」要在路由之前判定：每次命中消耗一次配额
    if path == "/backend-api/f/conversation"
        && let Some(Fault::FailFirstConversations { status, .. }) = current_fault()
    {
        let left = REMAINING_FAILURES.load(std::sync::atomic::Ordering::SeqCst);
        if left > 0 {
            REMAINING_FAILURES.store(left - 1, std::sync::atomic::Ordering::SeqCst);
            let code = axum::http::StatusCode::from_u16(status)
                .unwrap_or(axum::http::StatusCode::UNAUTHORIZED);
            return (code, "{\"detail\":\"mock: fail-first\"}".to_string()).into_response();
        }
    }
    match current_fault() {
        Some(Fault::Status { path_contains, status, body }) if path.contains(path_contains) => {
            let code = axum::http::StatusCode::from_u16(status)
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            return (code, body.to_string()).into_response();
        }
        Some(Fault::TruncateSse(limit)) if path == "/backend-api/f/conversation" => {
            let cut = CONVERSATION_SSE.len().min(limit);
            return (
                [("content-type", "text/event-stream")],
                CONVERSATION_SSE[..cut].to_string(),
            )
                .into_response();
        }
        _ => {}
    }
    match path.as_str() {
        "/" => HOME_HTML.to_string().into_response(),
        p if p.ends_with("/sentinel/chat-requirements/prepare") => json(PREPARE_JSON),
        p if p.ends_with("/sentinel/chat-requirements/finalize") => json(FINALIZE_JSON),
        "/backend-api/f/conversation/prepare" => json(PREPARE_CONVERSATION_JSON),
        // 图文对话的上传三步：申请上传位 / 传字节 / 确认
        p if p.starts_with("/backend-api/files") && !p.ends_with("/download") && p.ends_with("/uploaded") => {
            json_text("{}".to_string())
        }
        "/backend-api/files" => {
            let host = request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("127.0.0.1");
            json_text(format!(
                "{{\"file_id\":\"{UPLOAD_FILE_ID}\",\"upload_url\":\"http://{host}{UPLOAD_PATH}\"}}"
            ))
        }
        UPLOAD_PATH => (axum::http::StatusCode::OK, "").into_response(),
        "/backend-api/f/conversation" => {
            // 图片链路一定带 conduit token，文本链路一定不带
            let is_image = request.headers().contains_key("x-conduit-token");
            let body = match (is_image, current_fault()) {
                (false, _) => CONVERSATION_SSE.to_string(),
                // 两种「只回文本」故障的 SSE 形态相同，区别只在会话详情里有没有图
                (true, Some(Fault::ImageTextOnly | Fault::ImageTextOnlyWithDetailImage)) => {
                    image_text_only_sse()
                }
                (true, _) => image_sse(),
            };
            ([("content-type", "text/event-stream")], body).into_response()
        }
        p if p.starts_with("/backend-api/files/") && p.ends_with("/download") => {
            let host = request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("127.0.0.1");
            json_text(format!(
                "{{\"download_url\":\"http://{host}{IMAGE_ASSET_PATH}\"}}"
            ))
        }
        // 会话详情：只在那个故障下才有图，别的用例走不到这里
        p if p.starts_with("/backend-api/conversation/")
            && matches!(current_fault(), Some(Fault::ImageTextOnlyWithDetailImage)) =>
        {
            json_text(conversation_detail_with_image())
        }
        IMAGE_ASSET_PATH => ([("content-type", "image/png")], png_bytes()).into_response(),
        other => (
            axum::http::StatusCode::NOT_FOUND,
            format!("mock 未定义该路径: {other}"),
        )
            .into_response(),
    }
}

fn json_text(body: String) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
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

/// 注入给 mock 的故障。
///
/// 全部按「路径包含某片段」匹配，这样同一套断言可以复用于不同端点。
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    /// 命中路径返回该状态码与响应体。
    Status {
        /// 路径包含该片段即命中。
        path_contains: &'static str,
        /// 返回的状态码。
        status: u16,
        /// 返回的响应体。
        body: &'static str,
    },
    /// 会话 SSE 只发前 N 字节就以 200 结束（模拟中途断流）。
    TruncateSse(usize),
    /// 图片会话只回助手文本、不给任何图片指针（上游拒绝或解释）。
    ImageTextOnly,
    /// 图片会话 SSE 只有文本，但会话详情里其实有图。
    ///
    /// 用来说明「没图」必须真的确认过，不能看 SSE 里没有指针就判拒绝。
    ImageTextOnlyWithDetailImage,
    /// 不注入故障，只占住串行锁。
    ///
    /// mock 是进程内全局的：不带故障的用例也要持锁，否则会和注入故障的用例
    /// 在同一进程里并发跑，彼此看到对方的响应。
    Healthy,
    /// 前 `times` 次会话请求返回 401，之后恢复正常。
    ///
    /// 用来验证「换号重试」：第一个账号吃到 401，网关应换下一个账号重试。
    /// 401 带 `blames_account()` 语义，所以换号之后还要能落账。
    FailFirstConversations {
        /// 失败次数。
        times: usize,
        /// 返回的状态码。
        status: u16,
    },
}

static FAULT: Mutex<Option<Fault>> = Mutex::new(None);

/// `FailFirstConversations` 的剩余失败次数。
static REMAINING_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn current_fault() -> Option<Fault> {
    FAULT.lock().ok().and_then(|guard| *guard)
}

fn set_fault(fault: Option<Fault>) {
    if let Ok(mut guard) = FAULT.lock() {
        *guard = fault;
    }
    let remaining = match fault {
        Some(Fault::FailFirstConversations { times, .. }) => times,
        _ => 0,
    };
    REMAINING_FAILURES.store(remaining, std::sync::atomic::Ordering::SeqCst);
}

/// 故障注入期间持有的串行令牌；**丢弃时自动清除故障**。
pub struct FaultGuard {
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for FaultGuard {
    fn drop(&mut self) {
        set_fault(None);
    }
}

/// 注入一个故障，返回的守卫存活期间生效。
///
/// 同一进程里只有一台 mock 服务，所以故障是全局状态：用这把锁把多个故障用例串起来，
/// 守卫 Drop 时清空 —— 即使断言 panic 也不会把故障留给下一个用例。
pub async fn inject(fault: Fault) -> FaultGuard {
    static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let serial = SERIAL.get_or_init(|| tokio::sync::Mutex::new(()));
    let guard = serial.lock().await;
    set_fault(Some(fault));
    FaultGuard { _serial: guard }
}

static STARTED: OnceLock<MockUpstream> = OnceLock::new();

/// 起 mock 上游，并把进程里的上游站点指过来。
///
/// 需要 tokio 运行时。进程内只真正启动一次。
pub async fn spawn() -> MockUpstream {
    if let Some(existing) = STARTED.get() {
        return existing.clone();
    }
    let hits: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    // 兜底路由接住所有路径：图片链路的下载地址是动态的，逐个注册不现实
    let app = Router::new()
        .route("/", get(handle).post(handle))
        .fallback(handle)
        .with_state(hits.clone());

    // 服务必须活过**任何一个**用例的运行时：用例各自 new 一个 Runtime，
    // 结束时整棵任务树被丢弃。所以这里独占一个线程 + 自己的运行时，活到进程退出。
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("mock 上游应能绑定端口");
    std_listener
        .set_nonblocking(true)
        .expect("mock 上游应能设为非阻塞");
    let addr = std_listener.local_addr().expect("本地地址");

    std::thread::Builder::new()
        .name("mock-upstream".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("mock 上游运行时");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener)
                    .expect("mock 上游监听器");
                axum::serve(listener, app)
                    .await
                    .expect("mock 上游应持续运行");
            });
        })
        .expect("mock 上游线程");

    let base = format!("http://{addr}");
    point_upstream_at(&base);

    let mock = MockUpstream { base, hits };
    STARTED.set(mock.clone()).ok();
    mock
}
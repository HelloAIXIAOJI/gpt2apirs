//! wreq 封装的 TLS 指纹 HTTP 客户端 + 代理。
//!
//! - 未启用（默认）：本 crate 只提供骨架类型，workspace 可在无 BoringSSL/NASM 的环境下编译。
//! - 启用（`--features tls`）：引入 `wreq`，需要 BoringSSL 构建链。
//!   Windows 上还需安装 NASM（<https://www.nasm.us/>），否则 CMake 配置失败。

#![deny(unsafe_code)]
#![warn(missing_docs)]

/// 指纹目标。对应上游 `impersonate` 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Emulation {
    /// Chrome 110 —— 上游主链路默认使用。
    #[default]
    Chrome110,
    /// 最新 Chrome —— 备份与 OAuth 链路使用。
    ChromeLatest,
    /// Edge 101 —— 代理探测使用。
    Edge101,
}

impl Emulation {
    /// 返回 profile 名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Emulation::Chrome110 => "chrome_110",
            Emulation::ChromeLatest => "chrome_144",
            Emulation::Edge101 => "edge_101",
        }
    }
}

/// 客户端构造参数。
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    /// 指纹目标。
    pub emulation: Option<Emulation>,
    /// 代理 URL，例如 `http://127.0.0.1:7890` 或 `socks5://...`。
    pub proxy: Option<String>,
    /// 请求超时（秒）。
    pub timeout_secs: Option<u64>,
}

/// 字节流类型（SSE 增量读取用）。
///
/// 用 `Vec<u8>` 而不是 `bytes::Bytes`，避免为一个类型新增依赖。
pub type ByteStream =
    futures_util::stream::BoxStream<'static, anyhow::Result<Vec<u8>>>;

/// SSE 流式响应的开始状态。
pub struct SseStreamStart {
    /// HTTP 状态码。
    pub status: u16,
    /// 非 200 时的响应体（便于报错）。
    pub body_prefix: String,
    /// 200 时的字节流；非 200 为 `None`。
    pub stream: Option<ByteStream>,
}

/// HTTP 响应（状态码 + 文本体）。
#[derive(Debug, Clone)]
pub struct TextResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应文本。
    pub body: String,
}

/// HTTP 响应（状态码 + 二进制体）。
#[derive(Debug, Clone)]
pub struct BytesResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应字节。
    pub body: Vec<u8>,
}

// tls feature 下的真实实现

#[cfg(feature = "tls")]
mod imp {
    use std::time::Duration;

    use anyhow::{Context, Result};

    use super::{ByteStream, BytesResponse, ClientOptions, Emulation, SseStreamStart, TextResponse};

    /// 带 TLS 指纹的 HTTP 客户端。
    #[derive(Clone)]
    pub struct Client {
        inner: wreq::Client,
    }

    impl Client {
        /// 构造客户端。
        pub fn new(options: ClientOptions) -> Result<Self> {
            let emulation = options.emulation.unwrap_or_default();
            let timeout = Duration::from_secs(options.timeout_secs.unwrap_or(120));

            let mut builder = wreq::Client::builder()
                .emulation(match emulation {
                    Emulation::Chrome110 => wreq_util::Emulation::Chrome110,
                    Emulation::ChromeLatest => wreq_util::Emulation::Chrome124,
                    Emulation::Edge101 => wreq_util::Emulation::Edge101,
                })
                .timeout(timeout);

            if let Some(proxy) = options.proxy.as_deref() {
                let proxy = wreq::Proxy::all(proxy)
                    .with_context(|| format!("代理地址非法: {proxy}"))?;
                builder = builder.proxy(proxy);
            }

            let inner = builder.build().context("构建 wreq 客户端失败")?;
            Ok(Self { inner })
        }

        /// GET，返回文本。
        pub async fn get_text(
            &self,
            url: &str,
            headers: &[(&str, String)],
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.get(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let response = request.send().await.with_context(|| format!("GET {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body })
        }

        /// POST JSON，返回文本。
        pub async fn post_json(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: &serde_json::Value,
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            // 先设头，最后再 .json(body)：.json 会 insert Content-Type，
            // 若先 .json 再追加 Content-Type 会产生重复头，导致上游解析不出 dict。
            let mut request = self.inner.post(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let request = request.json(body);
            let response = request.send().await.with_context(|| format!("POST {url} 失败"))?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body: text })
        }

        /// DELETE，返回文本。
        ///
        /// 后台要删官网会话与记忆，wreq 这边得有个对应方法。
        pub async fn delete_text(
            &self,
            url: &str,
            headers: &[(&str, String)],
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.delete(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("DELETE {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body })
        }

        /// PATCH，可带 JSON body，返回文本。
        ///
        /// 上游的记忆开关是 PATCH `/backend-api/settings/account_user_setting?feature=m3m&value=`，
        /// 参数全在 query 里，body 可以不给。
        pub async fn patch_text(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: Option<&str>,
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.patch(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            if let Some(payload) = body {
                request = request.header("content-type", "application/json");
                request = request.body(payload.to_string());
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("PATCH {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body })
        }

        /// PUT 原始字节，返回文本。
        ///
        /// 上游图片上传走 Azure Blob 预签名 URL，必须用 PUT + 原始字节，
        /// 且要带 `x-ms-blob-type: BlockBlob`。
        pub async fn put_bytes(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: Vec<u8>,
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.put(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let response = request
                .body(body)
                .send()
                .await
                .with_context(|| format!("PUT {url} 失败"))?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body: text })
        }

        /// POST 原始字节（自定义 Content-Type），返回文本。
        ///
        /// 语音转写要 multipart 上传，塞不进 `post_form`（只收 urlencoded）；
        /// 它回来的又是 JSON，所以按文本收。合成拿音频字节走 `get_bytes`。
        pub async fn post_raw(
            &self,
            url: &str,
            headers: &[(&str, String)],
            content_type: &str,
            body: Vec<u8>,
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.post(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let response = request
                .header("content-type", content_type)
                .body(body)
                .send()
                .await
                .with_context(|| format!("POST {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body })
        }

        /// POST 表单（`application/x-www-form-urlencoded`），返回文本。
        ///
        /// OAuth 的 token 端点只接受表单，不接受 JSON。
        pub async fn post_form(
            &self,
            url: &str,
            headers: &[(&str, String)],
            form: &[(&str, String)],
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            let mut request = self.inner.post(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let pairs: Vec<(&str, &str)> = form
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            let encoded = gpt2apirs_core::util::form_body(&pairs);
            let response = request
                .body(encoded)
                .send()
                .await
                .with_context(|| format!("POST(form) {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Ok(TextResponse { status, body })
        }

        /// POST JSON 并收集整个 SSE 流（一次性读完）。
        pub async fn post_sse(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: &serde_json::Value,
            timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            use futures_util::StreamExt;

            let mut request = self.inner.post(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let request = request.json(body);
            let response = request.send().await.with_context(|| format!("POST(SSE) {url} 失败"))?;
            let status = response.status().as_u16();
            if status != 200 {
                let text = response.text().await.unwrap_or_default();
                return Ok(TextResponse { status, body: text });
            }

            let mut collected = String::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.context("读取 SSE 分块失败")?;
                collected.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(TextResponse { status, body: collected })
        }

        /// POST JSON 并以**增量流**返回 SSE。
        ///
        /// 与 [`Self::post_sse`] 的区别：那个会读完整个响应体才返回，
        /// 这个立刻把字节流交出去 —— 上层可以边收边转发给客户端。
        pub async fn post_sse_stream(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: &serde_json::Value,
            timeout: Option<Duration>,
        ) -> Result<SseStreamStart> {
            use futures_util::StreamExt;

            let mut request = self.inner.post(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let request = request.json(body);
            let response = request
                .send()
                .await
                .with_context(|| format!("POST(SSE 流) {url} 失败"))?;
            let status = response.status().as_u16();
            if status != 200 {
                let text = response.text().await.unwrap_or_default();
                return Ok(SseStreamStart {
                    status,
                    body_prefix: text,
                    stream: None,
                });
            }
            let stream: ByteStream = response
                .bytes_stream()
                .map(|chunk| match chunk {
                    Ok(bytes) => Ok(bytes.to_vec()),
                    Err(error) => Err(anyhow::anyhow!(error)),
                })
                .boxed();
            Ok(SseStreamStart {
                status,
                body_prefix: String::new(),
                stream: Some(stream),
            })
        }

        /// GET，返回原始字节。
        pub async fn get_bytes(
            &self,
            url: &str,
            headers: &[(&str, String)],
            timeout: Option<Duration>,
        ) -> Result<BytesResponse> {
            let mut request = self.inner.get(url);
            if let Some(limit) = timeout {
                request = request.timeout(limit);
            }
            for (name, value) in headers {
                request = request.header(*name, value.clone());
            }
            let response = request.send().await.with_context(|| format!("GET {url} 失败"))?;
            let status = response.status().as_u16();
            let body = response.bytes().await.unwrap_or_default().to_vec();
            Ok(BytesResponse { status, body })
        }
    }
}

// 无 tls feature 时的存根（保证 workspace 可编译）

#[cfg(not(feature = "tls"))]
mod imp {
    use std::time::Duration;

    use anyhow::{bail, Result};
    use serde_json::Value;

    use super::{BytesResponse, ClientOptions, SseStreamStart, TextResponse};

    /// 未启用 `tls` feature 时的存根客户端，所有请求都会失败。
    #[derive(Debug, Default, Clone)]
    pub struct Client;

    impl Client {
        /// 构造存根客户端。
        pub fn new(_options: ClientOptions) -> Result<Self> {
            tracing::warn!(
                "gpt2apirs-http built without `tls` feature: \
                 Client is a stub, no network requests are possible"
            );
            Ok(Self)
        }

        /// 总是失败。
        pub async fn get_text(&self, _url: &str, _h: &[(&str, String)], _t: Option<Duration>) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn post_sse_stream(
            &self,
            _url: &str,
            _headers: &[(&str, String)],
            _body: &serde_json::Value,
            _timeout: Option<Duration>,
        ) -> Result<SseStreamStart> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn put_bytes(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _body: Vec<u8>,
            _timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn post_form(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _form: &[(&str, String)],
            _timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn post_json(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _body: &Value,
            _timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn post_sse(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _body: &Value,
            _timeout: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn get_bytes(&self, _url: &str, _h: &[(&str, String)], _t: Option<Duration>) -> Result<BytesResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn post_raw(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _ct: &str,
            _b: Vec<u8>,
            _t: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn patch_text(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _b: Option<&str>,
            _t: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }

        /// 总是失败。
        pub async fn delete_text(
            &self,
            _url: &str,
            _h: &[(&str, String)],
            _t: Option<Duration>,
        ) -> Result<TextResponse> {
            bail!("gpt2apirs-http 未启用 tls feature")
        }
    }
}

pub use imp::Client;

/// 伪造浏览器请求头（不含鉴权）。
///
/// 这些头与上游真实浏览器的 `session.headers` 对齐。
pub fn browser_headers(user_agent: &str) -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", user_agent.to_string()),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8,en-US;q=0.7".to_string()),
        ("Cache-Control", "no-cache".to_string()),
        ("Pragma", "no-cache".to_string()),
        ("Priority", "u=1, i".to_string()),
        ("Sec-Ch-Ua", "\"Microsoft Edge\";v=\"143\", \"Chromium\";v=\"143\", \"Not A(Brand\";v=\"24\"".to_string()),
        ("Sec-Ch-Ua-Arch", "\"x86\"".to_string()),
        ("Sec-Ch-Ua-Bitness", "\"64\"".to_string()),
        ("Sec-Ch-Ua-Full-Version", "\"143.0.3650.96\"".to_string()),
        ("Sec-Ch-Ua-Mobile", "?0".to_string()),
        ("Sec-Ch-Ua-Model", "\"\"".to_string()),
        ("Sec-Ch-Ua-Platform", "\"Windows\"".to_string()),
        ("Sec-Ch-Ua-Platform-Version", "\"19.0.0\"".to_string()),
        ("Sec-Fetch-Dest", "empty".to_string()),
        ("Sec-Fetch-Mode", "cors".to_string()),
        ("Sec-Fetch-Site", "same-origin".to_string()),
    ]
}

/// 构造「已登录 backend-api」请求头。
///
/// 上游要求 web 端带 `X-OpenAI-Target-Path` / `X-OpenAI-Target-Route`，
/// 缺失会被判为非法来源。这里统一收口，避免各处重复拼装。
pub fn api_headers(user_agent: &str, token: &str, path: &str) -> Vec<(&'static str, String)> {
    let mut headers = browser_headers(user_agent);
    headers.push(("Accept", "*/*".to_string()));
    headers.push(("Authorization", format!("Bearer {token}")));
    headers.push(("Origin", "https://chatgpt.com".to_string()));
    headers.push(("Referer", "https://chatgpt.com/".to_string()));
    headers.push(("OAI-Device-Id", uuid_v4()));
    headers.push(("OAI-Session-Id", uuid_v4()));
    headers.push(("OAI-Language", "zh-CN".to_string()));
    headers.push(("OAI-Client-Version", CLIENT_VERSION.to_string()));
    headers.push(("OAI-Client-Build-Number", CLIENT_BUILD_NUMBER.to_string()));
    headers.push(("X-OpenAI-Target-Path", path.to_string()));
    headers.push(("X-OpenAI-Target-Route", path.to_string()));
    headers
}

/// 生成一个 v4 UUID（用于设备/会话 ID）。
///
/// 手写而不引 uuid crate：http 层只需这一个能力，不值得多一个依赖。
fn uuid_v4() -> String {
    // 时间 + 进程内计数器 + 地址扰动；不追求密码学强度，只为每次请求不同。
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = |mut x: u64| {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x
    };
    let a = mix(now ^ seq.rotate_left(17));
    let b = mix(now.rotate_left(29) ^ seq.wrapping_mul(0x9e3779b97f4a7c15));
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// 默认浏览器 User-Agent。
///
/// 与 `browser_headers` 里的 `Sec-Ch-Ua` 声称的 Edge 143 保持一致 ——
/// UA 与 client hints 必须自洽，否则上游会把请求判为异常客户端。
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";

/// OpenAI 客户端版本号（对应 Python `DEFAULT_CLIENT_VERSION`）。
pub const CLIENT_VERSION: &str = "prod-a194cd50d4416d3c0b47c740f206b12ce60f5887";

/// OpenAI 客户端构建号（对应 Python `DEFAULT_CLIENT_BUILD_NUMBER`）。
pub const CLIENT_BUILD_NUMBER: &str = "6708908";
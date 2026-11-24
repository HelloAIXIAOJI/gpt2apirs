//! 调用日志中间件：把每次对外请求写进 `call_records`。
//!
//! 只记业务端点；健康检查与静态图片不记，否则日志会被心跳淹没。

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use gpt2apirs_storage::repo::{CallRecordRepo, NewCallRecord};

use crate::error::ErrorCodeTag;
use crate::state::AppState;

tokio::task_local! {
    /// 当前请求的附加信息。handler 用它上报「这次用了哪个账号」。
    static CURRENT_CALL: Arc<CallMeta>;
}

/// 当前请求的附加信息。
#[derive(Debug, Default)]
pub struct CallMeta {
    account_id: std::sync::Mutex<Option<uuid::Uuid>>,
}

impl CallMeta {
    /// 记录本次请求实际服务的账号。
    pub fn note_account(&self, account_id: uuid::Uuid) {
        if let Ok(mut slot) = self.account_id.lock() {
            *slot = Some(account_id);
        }
    }

    /// 读取已记录的账号。
    pub fn account_id(&self) -> Option<uuid::Uuid> {
        self.account_id.lock().ok().and_then(|slot| *slot)
    }
}

/// 供 handler 调用：上报本次请求使用的账号。
///
/// 不在请求作用域内时静默忽略（例如被单独调用的内部函数）。
pub fn note_account(account_id: uuid::Uuid) {
    let _ = CURRENT_CALL.try_with(|meta| meta.note_account(account_id));
}

/// 不记录的路径前缀。
const SKIP_PREFIXES: [&str; 4] = [
    "/health",
    "/version",
    "/images/",
    "/favicon",
];

/// 是否记录该路径。
pub fn should_record(path: &str) -> bool {
    !SKIP_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(prefix))
}

/// 是否把这次请求记进调用日志。
///
/// 跨站预检（OPTIONS）不算业务调用：每个前端请求都可能先来一发，
/// 记进来只会把日志冲成一片 OPTIONS。
pub fn should_record_call(method: &str, path: &str) -> bool {
    method != "OPTIONS" && should_record(path)
}

/// 取调用方 IP：优先 `X-Forwarded-For` 第一跳，其次 `X-Real-IP`。
///
/// 反代后面这两个头才有意义；直连时都取不到，留空即可（别拿连接信息凑，
/// 在反代下那是代理自己的地址，反而误导）。
fn client_ip(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = header_text(headers, "x-forwarded-for") {
        if let Some(first) = value.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    header_text(headers, "x-real-ip")
}

/// 只留 API key 的前缀：`Bearer sk-g2a-abcd1234ef` → `sk-g2a-abcd`。
///
/// 日志里绝不能出现完整 key —— 有日志读取权限的人不该拿到能直接用的凭据。
fn key_prefix(raw: &str) -> Option<String> {
    let value = raw.trim();
    let value = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or(value)
        .trim();
    if value.is_empty() {
        return None;
    }
    let prefix: String = value.chars().take(12).collect();
    if value.chars().count() > 12 {
        Some(format!("{prefix}…"))
    } else {
        // 太短的串补省略号会显得像完整 key，直接原样留（它也不可能是真 key）
        Some(prefix)
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// axum 中间件。
pub async fn record_call(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let request_id = uuid::Uuid::new_v4();
    // 详情页要能回答「谁调过来的」：请求在 next.run 之后就拿不到头了，先取样。
    let client_ip = client_ip(request.headers());
    let user_agent = header_text(request.headers(), header::USER_AGENT.as_str());
    let api_key_prefix = header_text(request.headers(), header::AUTHORIZATION.as_str())
        .and_then(|value| key_prefix(&value));

    let meta = Arc::new(CallMeta::default());
    let mut response = CURRENT_CALL.scope(meta.clone(), next.run(request)).await;
    let status = response.status().as_u16() as i32;
    let duration_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
    let tag = response.extensions().get::<ErrorCodeTag>().cloned();
    let error_code = tag.as_ref().map(|tag| tag.code.clone());
    let error_message = tag.as_ref().map(|tag| tag.message.clone());

    if let Ok(value) = request_id.to_string().parse() {
        response.headers_mut().insert("x-request-id", value);
    }

    if should_record_call(&method, &path) {
        let repo = CallRecordRepo::new(state.db.clone());
        let record = NewCallRecord {
            method,
            path,
            status,
            duration_ms,
            account_id: meta.account_id(),
            request_id: Some(request_id),
            error_code,
            client_ip,
            user_agent,
            api_key_prefix,
            error_message,
        };
        // 落库失败不该影响响应：只记日志，不向上抛
        tokio::spawn(async move {
            if let Err(error) = repo.insert(&record).await {
                tracing::warn!(%error, "写调用日志失败");
            }
        });
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_health_and_static() {
        assert!(!should_record("/health"));
        assert!(!should_record("/health/ready"));
        assert!(!should_record("/version"));
        assert!(!should_record("/images/2026/11/13/a.png"));
    }

    #[test]
    fn records_business_endpoints() {
        assert!(should_record("/v1/chat/completions"));
        assert!(should_record("/api/accounts"));
        assert!(should_record("/admin/login"));
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn client_ip_prefers_forwarded_first_hop() {
        let map = headers(&[("x-forwarded-for", "1.2.3.4, 10.0.0.1"), ("x-real-ip", "9.9.9.9")]);
        assert_eq!(client_ip(&map).as_deref(), Some("1.2.3.4"));
        let only_real = headers(&[("x-real-ip", "9.9.9.9")]);
        assert_eq!(client_ip(&only_real).as_deref(), Some("9.9.9.9"));
        assert_eq!(client_ip(&HeaderMap::new()), None, "直连时留空，别拿代理地址凑");
    }

    #[test]
    fn key_prefix_never_keeps_the_whole_key() {
        // 完整 key 绝不能进日志：只留前 12 个字符
        let full = "Bearer sk-g2a-d5116c6de4f75fa63a338fbcf2b4e2189ed71b591ad7d8be";
        let prefix = key_prefix(full).expect("应能取到前缀");
        assert_eq!(prefix, "sk-g2a-d5116…", "保留前 12 个字符再加省略号");
        assert!(!prefix.contains("ad7d8be"), "前缀里不许出现 key 的尾部");
        assert_eq!(key_prefix("   "), None);
        assert_eq!(key_prefix("abc").as_deref(), Some("abc"), "太短的串原样留");
    }

    #[test]
    fn preflight_is_not_a_call() {
        assert!(should_record_call("GET", "/v1/models"));
        assert!(should_record_call("POST", "/v1/chat/completions"));
        assert!(!should_record_call("OPTIONS", "/v1/models"));
        assert!(!should_record_call("OPTIONS", "/health"));
    }
}

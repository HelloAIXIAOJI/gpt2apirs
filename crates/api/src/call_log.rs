//! 调用日志中间件：把每次对外请求写进 `call_records`。
//!
//! 只记业务端点；健康检查与静态图片不记，否则日志会被心跳淹没。

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use gpt2apirs_storage::repo::{CallRecordRepo, NewCallRecord};

use crate::error::ErrorCodeTag;
use crate::state::AppState;

/// 不记录的路径前缀。
const SKIP_PREFIXES: [&str; 4] = ["/health", "/version", "/images/", "/favicon"];

/// 是否记录该路径。
pub fn should_record(path: &str) -> bool {
    !SKIP_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(prefix))
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

    let mut response = next.run(request).await;
    let status = response.status().as_u16() as i32;
    let duration_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
    let error_code = response
        .extensions()
        .get::<ErrorCodeTag>()
        .map(|tag| tag.0.clone());

    if let Ok(value) = request_id.to_string().parse() {
        response.headers_mut().insert("x-request-id", value);
    }

    if should_record(&path) {
        let repo = CallRecordRepo::new(state.db.clone());
        let record = NewCallRecord {
            method,
            path,
            status,
            duration_ms,
            account_id: None,
            request_id: Some(request_id),
            error_code,
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
}

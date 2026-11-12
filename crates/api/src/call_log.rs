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
const SKIP_PREFIXES: [&str; 5] = [
    "/health",
    "/version",
    "/images/",
    "/admin/assets/",
    "/favicon",
];

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

    let meta = Arc::new(CallMeta::default());
    let mut response = CURRENT_CALL.scope(meta.clone(), next.run(request)).await;
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
            account_id: meta.account_id(),
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

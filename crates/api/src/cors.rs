//! 跨站（CORS）策略。
//!
//! 浏览器里的前端工具直接调 `/v1/*` 时，带 `Authorization` 的请求会先发预检（OPTIONS）。
//! 没有 CORS 响应头，浏览器会把整个请求判死（`No 'Access-Control-Allow-Origin' header`），
//! 于是这类工具只能绕到服务端转发才用得起来。
//!
//! 只挂在**对外接口**上（`/v1/*`、`/api/image-tasks*`）：它们认 `Authorization: Bearer`，
//! 不依赖 cookie，所以缺省放开任意来源是安全的。后台管理面用会话 cookie，
//! 不挂 CORS —— 那等于给跨站页面开一扇带凭据的门。

use std::time::Duration;

use axum::http::{HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// 预检结果缓存时长。浏览器据此减少 OPTIONS 次数。
const MAX_AGE_SECS: u64 = 600;

/// 允许的跨站方法。放开 OPTIONS 是预检本身要用的。
const METHODS: [Method; 6] = [
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::OPTIONS,
];

/// 构造对外接口的 CORS 层。
///
/// `allow_origins` 为空表示放开任意来源；非空则按精确匹配白名单。
/// 请求头不枚举（网关要兼容各家 SDK），预检时按请求声明的头原样回显。
pub fn layer(allow_origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods(METHODS)
        .allow_headers(tower_http::cors::Any)
        .max_age(Duration::from_secs(MAX_AGE_SECS));

    if allow_origins.is_empty() {
        return layer.allow_origin(AllowOrigin::any());
    }

    // 白名单里写坏的项直接丢掉；全都写坏等于谁都不放行（失败方向是「更严」）。
    let origins: Vec<HeaderValue> = allow_origins
        .iter()
        .filter_map(|value| match HeaderValue::from_str(value) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                tracing::warn!(origin = %value, "CORS_ALLOW_ORIGINS 里的来源不合法，已忽略");
                None
            }
        })
        .collect();
    layer.allow_origin(AllowOrigin::list(origins))
}

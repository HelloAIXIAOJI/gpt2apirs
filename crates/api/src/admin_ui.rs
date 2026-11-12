//! 后台前端页面。
//!
//! 手写 HTML/CSS/JS，编译期用 `include_str!` 打进二进制：
//! 部署只有一个可执行文件，不需要 Node 构建链。
//!
//! 页面本身是公开的（只是个空壳）；数据全靠会话 cookie 调 `/api/*` 拿。

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../assets/admin/index.html");
const STYLE_CSS: &str = include_str!("../assets/admin/style.css");
const APP_JS: &str = include_str!("../assets/admin/app.js");

/// `GET /admin` —— 返回单页外壳。
pub async fn index() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"),
         (header::CACHE_CONTROL, "no-cache")],
        INDEX_HTML,
    )
        .into_response()
}

/// `GET /admin/assets/{path}` —— 返回静态资源。
pub async fn asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let (body, mime) = match name.as_str() {
        "style.css" => (STYLE_CSS, "text/css; charset=utf-8"),
        "app.js" => (APP_JS, "application/javascript; charset=utf-8"),
        _ => {
            return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "not found")
                .into_response();
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_references_assets_and_has_no_placeholder_text() {
        assert!(INDEX_HTML.contains("/admin/assets/app.js"));
        assert!(INDEX_HTML.contains("/admin/assets/style.css"));
        assert!(INDEX_HTML.contains("登录"), "必须有登录面板");
    }

    #[test]
    fn script_uses_real_endpoints() {
        for endpoint in ["'/admin/session'", "'/api/metrics", "'/api/accounts'", "'/api/logs", "'/api/images", "'/api/keys'"] {
            assert!(APP_JS.contains(endpoint), "前端应调用 {endpoint}");
        }
    }

    #[test]
    fn script_has_no_fake_data() {
        for marker in ["mock", "fakeData", "placeholder="] {
            assert!(!APP_JS.to_lowercase().contains(&marker.to_lowercase()), "不许出现 {marker}");
        }
    }
}

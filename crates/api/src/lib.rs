//! 对外 API：axum 路由、鉴权、错误响应格式。
//!
//! 路由清单见 `README.md` 的「对外接口」一节；设计见 `docs/m1-design.md`。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
pub mod call_log;
pub mod error;
pub mod routes;
pub mod session;
pub mod state;
pub mod static_files;
pub mod system;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, post, put};
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ErrorFormat};
pub use state::AppState;
pub use system::APP_VERSION;

/// 构建完整路由。
pub fn build_router(state: Arc<AppState>) -> Router {
    // /v1/* 只认 API key；后台登录口令在这里一定失败。
    let v1 = Router::new()
        .route("/v1/models", get(routes::v1_models::list_models))
        .route("/v1/images/generations", post(routes::v1_images::generate))
        .route("/v1/images/edits", post(routes::v1_image_edits::edit_images))
        .route("/v1/chat/completions", post(routes::v1_chat::chat_completions))
        .route("/v1/responses", post(routes::v1_responses::create_response))
        .route("/v1/messages", post(routes::v1_messages::create_message))
        .route("/v1/search", post(routes::v1_search::search))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    // 后台：登录口令（或会话 cookie）才能进。
    let admin = Router::new()
        .route("/api/ping", get(ping))
        .route("/admin/session", get(routes::admin::session_info))
        .route("/admin/logout", post(routes::admin::logout))
        .route(
            "/api/keys",
            get(routes::admin::list_keys).post(routes::admin::create_key),
        )
        .route("/api/keys/{id}", delete(routes::admin::revoke_key))
        .route("/api/logs", get(routes::logs::list_logs))
        .route("/api/metrics", get(routes::logs::metrics))
        .route("/api/settings", get(routes::settings::get_settings))
        .route("/api/settings", put(routes::settings::update_settings))
        .route(
            "/api/accounts",
            get(routes::accounts::list_accounts)
                .post(routes::accounts::create_accounts)
                .delete(routes::accounts::delete_accounts),
        )
        .route("/api/accounts/stats", get(routes::accounts::stats))
        .route("/api/accounts/update", post(routes::accounts::update_account))
        .route("/api/accounts/sync", post(routes::accounts::sync_accounts))
        .route(
            "/api/accounts/refresh-token",
            post(routes::accounts::refresh_tokens),
        )
        .route(
            "/api/accounts/operations/{id}",
            get(routes::accounts::get_operation),
        )
        .route(
            "/api/accounts/oauth/start",
            post(routes::accounts::oauth_start),
        )
        .route(
            "/api/accounts/oauth/finish",
            post(routes::accounts::oauth_finish),
        )
        .route(
            "/api/accounts/export",
            post(routes::accounts::export_accounts),
        )
        .route(
            "/api/accounts/{id}/test",
            post(routes::accounts::test_account),
        )
        .route("/api/accounts/{id}", delete(routes::accounts::delete_accounts))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));

    // 登录接口本身不能要求已登录。
    let login = Router::new().route("/admin/login", post(routes::admin::login));

    let public = Router::new()
        .route("/health", get(system::health))
        .route("/health/ready", get(system::ready))
        .route("/version", get(system::version))
        .route("/images/{*path}", get(static_files::serve_image));

    public
        .merge(v1)
        .merge(admin)
        .merge(login)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            call_log::record_call,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 占位端点：验证鉴权中间件链路通。
async fn ping() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "pong": true }))
}

/// 统一 404。
async fn not_found() -> ApiError {
    ApiError::openai(gpt2apirs_core::AppError::NotFound {
        message: "Not Found".to_string(),
    })
}

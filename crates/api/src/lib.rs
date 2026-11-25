//! 对外 API：axum 路由、鉴权、错误响应格式。
//!
//! 路由清单见 `README.md` 的「对外接口」一节；设计见 `docs/m1-design.md`。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
pub mod call_log;
pub mod clearance;
pub mod cors;
pub mod error;
pub mod image_archive;
pub mod image_response;
pub mod images_download;
pub mod image_tasks;
pub mod admin;
pub mod library;
pub mod login_throttle;
pub mod proxy_service;
pub mod references;
pub mod routes;
pub mod session;
pub mod state;
pub mod static_files;
pub mod thumbnails;
pub mod system;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, patch, post, put};
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ErrorFormat};
pub use state::AppState;
pub use system::APP_VERSION;

/// 构建完整路由。
pub fn build_router(state: Arc<AppState>) -> Router {
    // 对外接口要能被浏览器前端直接调用：CORS 层必须在鉴权中间件**外面**，
    // 否则预检（OPTIONS）会先撞上「没有 API key」的 401。
    let cors_layer = cors::layer(&state.config.cors_allow_origins);

    // /v1/* 只认 API key；后台登录口令在这里一定失败。
    let v1 = Router::new()
        .route("/v1/models", get(routes::v1_models::list_models))
        .route("/v1/models/{id}", get(routes::v1_models::get_model))
        .route("/v1/images/generations", post(routes::v1_images::generate))
        // 参考图上限由运行设置控制（默认 10 MB，可调），所以这里把 body 上限放宽到 64 MB，
        // 真正的大小判断在 handler 里流式做；否则 axum 的默认上限会先报一句看不懂的解析错误。
        .route(
            "/v1/images/edits",
            post(routes::v1_image_edits::edit_images)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/v1/chat/completions", post(routes::v1_chat::chat_completions))
        .route("/v1/responses", post(routes::v1_responses::create_response))
        .route("/v1/messages", post(routes::v1_messages::create_message))
        .route(
            "/v1/messages/count_tokens",
            post(routes::v1_messages::count_tokens),
        )
        .route("/v1/search", post(routes::v1_search::search))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ))
        .layer(cors_layer.clone());

    // 后台：登录口令（或会话 cookie）才能进。
    let admin = Router::new()
        .route("/api/ping", get(ping))
        .route("/admin/session", get(routes::admin::session_info))
        .route("/admin/logout", post(routes::admin::logout))
        .route(
            "/api/keys",
            get(routes::admin::list_keys).post(routes::admin::create_key),
        )
        .route("/api/keys/{id}", patch(routes::admin::update_key).delete(routes::admin::revoke_key))
        .route("/api/keys/{id}/restore", post(routes::admin::restore_key))
        .route(
            "/api/images",
            get(routes::images::list_images),
        )
        .route("/api/images/stats", get(routes::images::image_stats))
        .route("/api/backup", post(routes::backup::create_backup))
        .route("/api/backups", get(routes::backup::list_backups))
        .route("/api/restore", post(routes::backup::restore))
        .route(
            "/api/images/{id}",
            delete(routes::images::delete_image),
        )
        // 管理面看同一批异步生图任务：客户端走 API key 的 /api/image-tasks，
        // 后台走会话鉴权的 /api/admin/image-tasks，两条路由共用 handler。
        .route(
            "/api/admin/image-tasks",
            get(routes::image_tasks::list).post(routes::image_tasks::submit),
        )
        .route(
            "/api/admin/image-tasks/{id}",
            delete(routes::image_tasks::delete),
        )
        .route("/api/images/archive", post(admin::gallery::archive_api))
        .route(
            "/api/images/references/gc",
            post(routes::images::gc_references),
        )
        .route("/api/proxy/status", get(routes::proxy::status))
        .route("/api/proxy/start", post(routes::proxy::start))
        .route("/api/proxy/refresh", post(routes::proxy::refresh))
        .route("/api/proxy/stop", post(routes::proxy::stop))
        .route("/api/proxy/nodes", get(routes::proxy::nodes))
        .route("/api/proxy/select", post(routes::proxy::select))
        .route("/api/proxy/probe", post(routes::proxy::probe))
        .route("/api/proxy/clearance", post(routes::proxy::clearance))
        .route("/api/proxy/ping", post(routes::proxy::ping))
        .route("/api/buckets", get(routes::buckets::list))
        .route("/api/buckets", post(routes::buckets::create))
        .route(
            "/api/buckets/{id}",
            patch(routes::buckets::update).delete(routes::buckets::remove),
        )
        .route("/api/logs", get(routes::logs::list_logs))
        .route("/api/conversations", get(routes::conversations::list))
        .route("/api/conversations/{id}", delete(routes::conversations::remove))
        .route("/api/memories", get(routes::memories::list))
        .route("/api/memories/{id}", delete(routes::memories::remove))
        .route("/api/logs/{id}", get(routes::logs::get_log).delete(routes::logs::delete_log))
        .route("/api/logs/delete", post(routes::logs::delete_logs))
        .route("/api/metrics", get(routes::logs::metrics))
        .route("/api/dashboard", get(routes::dashboard::dashboard))
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
        .route(
            "/api/accounts/{id}",
            get(routes::accounts::get_account).delete(routes::accounts::delete_account),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));

    // 异步生图任务：给 API 调用方，按 API key 鉴权（不是后台管理面）。
    let image_task_routes = Router::new()
        .route("/api/image-tasks", get(routes::image_tasks::list))
        .route("/api/image-tasks/generations", post(routes::image_tasks::submit))
        .route(
            "/api/image-tasks/edits",
            post(routes::image_tasks::submit_edit)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/image-tasks/{id}",
            get(routes::image_tasks::get_one).delete(routes::image_tasks::delete),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ))
        .layer(cors_layer.clone());

    // 登录接口本身不能要求已登录。
    let login = Router::new().route(
        "/admin/login",
        post(routes::admin::login).layer(axum::middleware::from_fn_with_state(
            state.clone(),
            login_throttle::guard,
        )),
    );

    // 页面：登录页与概览页自己判会话；数据接口仍然只有 /api/* 的中间件放行。
    let ui = Router::new()
        .route("/", get(admin::root))
        .route("/overview", get(admin::overview))
        .route("/login", post(admin::login))
        .route("/logout", post(admin::logout))
        .route("/admin/assets/htmx.min.js", get(admin::htmx_asset))
        .route("/admin/assets/admin.css", get(admin::css_asset))
        .route("/admin/assets/glightbox.js", get(admin::glightbox_js_asset))
        .route("/admin/assets/glightbox.css", get(admin::glightbox_css_asset))
        .route("/accounts", get(admin::accounts::index))
        // 桶已并入账号首页；老书签重定向过去，别留死链
        .route(
            "/buckets",
            get(|| async { axum::response::Redirect::to("/accounts") }),
        )
        .route("/buckets/create", post(admin::buckets::create))
        .route("/buckets/{id}/rename", post(admin::buckets::rename))
        .route("/buckets/{id}/egress", post(admin::buckets::set_egress))
        .route("/buckets/{id}/delete", post(admin::buckets::delete))
        .route("/settings", get(admin::settings::index).post(admin::settings::save))
        .route("/logs", get(admin::logs::index))
        .route("/conversations", get(admin::conversations::index))
        .route("/conversations/delete", post(admin::conversations::remove))
        .route(
            "/conversations/{id}/delete",
            post(admin::conversations::remove_one),
        )
        .route("/memories", get(admin::memories::index))
        .route("/memories/delete", post(admin::memories::remove))
        .route("/memories/{id}/delete", post(admin::memories::remove_one))
        .route("/logs/{id}", get(admin::logs::detail))
        .route("/logs/{id}/delete", post(admin::logs::remove))
        .route("/logs/delete", post(admin::logs::remove_matching))
        .route("/gallery", get(admin::gallery::index))
        .route("/gallery/archive", post(admin::gallery::archive))
        .route("/gallery/gc", post(admin::gallery::gc))
        .route("/gallery/panel", get(admin::gallery::panel))
        .route("/backups", get(admin::backups::index))
        .route("/backups/create", post(admin::backups::create))
        .route("/backups/restore", post(admin::backups::restore))
        .route("/backups/delete", post(admin::backups::delete))
        .route("/backups/download/{file}", get(admin::backups::download))
        .route("/tasks", get(admin::tasks::index))
        .route("/tasks/panel", get(admin::tasks::panel))
        .route("/tasks/delete", post(admin::tasks::delete))
        .route("/proxy", get(admin::proxy::index))
        .route("/proxy/start", post(admin::proxy::start))
        .route("/proxy/start-cached", post(admin::proxy::start_cached))
        .route("/proxy/refresh", post(admin::proxy::refresh))
        .route("/proxy/stop", post(admin::proxy::stop))
        .route("/proxy/select", post(admin::proxy::select))
        .route("/proxy/probe", post(admin::proxy::probe))
        .route("/proxy/probe.json", post(admin::proxy::probe_json))
        .route("/proxy/clearance", post(admin::proxy::clearance))
        .route("/proxy/ping.json", post(admin::proxy::ping_json))
        .route("/keys", get(admin::keys::index))
        .route("/keys/create", post(admin::keys::create))
        .route("/keys/{id}/revoke", post(admin::keys::revoke))
        .route("/keys/{id}/restore", post(admin::keys::restore))
        .route("/keys/{id}/delete", post(admin::keys::delete))
        .route("/keys/batch", post(admin::keys::batch))
        .route("/keys/{id}/edit", get(admin::keys::edit_form).post(admin::keys::edit_submit))
        .route("/accounts/import", post(admin::accounts::import_accounts))
        .route("/accounts/batch", post(admin::accounts::batch))
        .route(
            "/accounts/bulk/preview",
            get(admin::accounts::bulk_preview),
        )
        .route("/accounts/oauth/start", post(admin::accounts::oauth_start_form))
        .route("/accounts/oauth/finish", post(admin::accounts::oauth_finish_form))
        .route("/accounts/operations/{id}", get(admin::accounts::operation_fragment))
        .route("/accounts/{id}/edit", get(admin::accounts::edit_form).post(admin::accounts::edit_submit))
        .route("/accounts/{id}/status", post(admin::accounts::toggle_status))
        .route("/accounts/{id}/delete", post(admin::accounts::delete_account))
        .route("/accounts/{id}/test", post(admin::accounts::test_account))
        .route("/accounts/{id}/refresh", post(admin::accounts::refresh_token));

    let public = Router::new()
        .route("/health", get(system::health))
        .route("/health/ready", get(system::ready))
        .route("/version", get(system::version))
        .route("/images/download", get(admin::gallery::download_all))
        .route("/images/{*path}", get(static_files::serve_image))
        .route("/image-thumbnails/{*path}", get(static_files::serve_thumbnail))
        // 图片是公开资源，前端要 fetch 成 blob / 画进 canvas 就得有 CORS
        .layer(cors_layer.clone());

    public
        .merge(ui)
        .merge(v1)
        .merge(admin)
        .merge(image_task_routes)
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
    ApiError::openai(gpt2apirs_core::AppError::not_found("Not Found"))
}

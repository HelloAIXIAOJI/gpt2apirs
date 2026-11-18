//! 对外接口的跨站（CORS）行为。
//!
//! 浏览器前端直接调 `/v1/*` 走的是「带 Authorization 的跨站请求」：
//! 先发 OPTIONS 预检，再发真请求。这两步都必须带 `Access-Control-Allow-Origin`，
//! 否则浏览器直接判死，跟服务端鉴权是否通过无关。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const TEST_KEY: &str = "test-admin-key-123456";
const CANVAS: &str = "https://canvas.evalfy.com";

fn state(extra: &[(&'static str, &'static str)]) -> Arc<AppState> {
    let mut map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("ADMIN_KEY", TEST_KEY),
    ]);
    for (key, value) in extra {
        map.insert(key, value);
    }
    let lookup = move |key: &str| {
        map.get(key)
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
    };
    let config = Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置应可构造"));
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable")
        .expect("lazy pool 应可构造");
    let account_pool = Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(pool.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ));
    Arc::new(AppState::new(
        config,
        Arc::new(RuntimeSettings::new(HashMap::new())),
        pool,
        Arc::new(
            gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default())
                .expect("客户端"),
        ),
        account_pool,
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(TEST_KEY, "test"),
        )),
    ))
}

/// 发一个请求，返回状态与响应头。
async fn send(state: Arc<AppState>, request: Request<Body>) -> (StatusCode, HeaderMap) {
    let response = build_router(state)
        .oneshot(request)
        .await
        .expect("请求应成功");
    let status = response.status();
    (status, response.headers().clone())
}

fn preflight(path: &str, origin: &str) -> Request<Body> {
    Request::builder()
        .method("OPTIONS")
        .uri(path)
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "authorization,content-type",
        )
        .body(Body::empty())
        .expect("构造预检请求")
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn preflight_on_v1_models_is_answered_without_api_key() {
    // 预检本身不带 Authorization：它必须先被 CORS 层接住，不能撞到 401
    let (status, headers) = send(state(&[]), preflight("/v1/models", CANVAS)).await;
    assert_eq!(status, StatusCode::OK, "预检应成功");
    assert_eq!(
        header_value(&headers, "access-control-allow-origin"),
        "*",
        "缺省放开任意来源"
    );
    assert!(
        header_value(&headers, "access-control-allow-methods").contains("POST"),
        "应声明允许的方法：{headers:?}"
    );
    let allowed = header_value(&headers, "access-control-allow-headers");
    assert!(
        allowed.contains("authorization") || allowed == "*",
        "应允许 Authorization 头：{allowed}"
    );
}

#[tokio::test]
async fn real_request_carries_allow_origin() {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header(header::ORIGIN, CANVAS)
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("构造请求");
    let (status, headers) = send(state(&[]), request).await;
    assert_eq!(status, StatusCode::OK, "带合法 key 应通过");
    assert_eq!(header_value(&headers, "access-control-allow-origin"), "*");
}

#[tokio::test]
async fn public_images_are_readable_cross_origin() {
    // 前端把生成的图 fetch 成 blob / 画进 canvas 时要能读；报错响应也得带头，
    // 否则浏览器只会给一个看不出原因的 opaque failure。
    let request = Request::builder()
        .method("GET")
        .uri("/image-thumbnails/nothing-here.webp")
        .header(header::ORIGIN, CANVAS)
        .body(Body::empty())
        .expect("构造请求");
    let (_, headers) = send(state(&[]), request).await;
    assert_eq!(header_value(&headers, "access-control-allow-origin"), "*");
}

#[tokio::test]
async fn admin_surface_stays_out_of_cors() {
    // 管理面用会话 cookie：给它开跨站等于开一扇带凭据的门
    let request = Request::builder()
        .method("GET")
        .uri("/api/ping")
        .header(header::ORIGIN, CANVAS)
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("构造请求");
    let (status, headers) = send(state(&[]), request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get("access-control-allow-origin").is_none(),
        "后台不应带 CORS 头：{headers:?}"
    );
}

#[tokio::test]
async fn whitelist_only_echoes_configured_origin() {
    let state = state(&[("CORS_ALLOW_ORIGINS", CANVAS)]);

    let (status, headers) = send(state.clone(), preflight("/v1/models", CANVAS)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header_value(&headers, "access-control-allow-origin"),
        CANVAS,
        "白名单里的来源应被回显"
    );

    let (status, headers) = send(state, preflight("/v1/models", "https://evil.example")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get("access-control-allow-origin").is_none(),
        "白名单外的来源不应拿到 CORS 头：{headers:?}"
    );
}

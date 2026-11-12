//! `/v1/images/edits` 的**输入解析与校验**测试。
//!
//! 全部离线：验证「multipart / JSON 两种形态都能解析」以及「非法请求被拒」。
//! 真正出图要靠真机（这里账号池是空的，走到取号就返回 503 —— 那正是本文件要覆盖的终点）。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const AUTH_KEY: &str = "test-admin-key-123456";
const BOUNDARY: &str = "----gpt2apirsTESTBOUNDARY";

fn account_pool(db: &sqlx::PgPool) -> Arc<gpt2apirs_account::AccountPool> {
    Arc::new(gpt2apirs_account::AccountPool::new(
        gpt2apirs_account::AccountRepo::new(db.clone()),
        gpt2apirs_account::PoolConfig::default(),
    ))
}

fn state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("AUTH_KEY", AUTH_KEY),
    ]);
    let lookup = move |key: &str| map.get(key).map(|v| v.to_string()).filter(|v| !v.is_empty());
    let config = Arc::new(BootstrapConfig::from_lookup(&lookup).expect("配置"));
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable")
        .expect("lazy pool");
    Arc::new(AppState::new(
        config,
        Arc::new(RuntimeSettings::new(HashMap::new())),
        pool.clone(),
        Arc::new(
            gpt2apirs_http::Client::new(gpt2apirs_http::ClientOptions::default()).expect("client"),
        ),
        account_pool(&pool),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
    ))
}

/// 一个最小的合法 PNG（2x2）。
fn tiny_png() -> Vec<u8> {
    // 手工构造，避免测试依赖 image crate 的编码器
    const B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAF0lEQVR42mP8z8Dwn4GBgYEIwAgAxD0DwQAAAABJRU5ErkJggg==";
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(B64)
        .expect("内置 PNG 应可解码")
}

enum Part {
    Text(&'static str, String),
    File(&'static str, &'static str, Vec<u8>),
}

fn multipart_body(parts: Vec<Part>) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::Text(name, value) => {
                out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                );
                out.extend_from_slice(value.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Part::File(name, filename, bytes) => {
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n\
                         Content-Type: image/png\r\n\r\n"
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(&bytes);
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

async fn post_multipart(parts: Vec<Part>, auth: Option<&str>) -> (StatusCode, Value) {
    let body = multipart_body(parts);
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/images/edits")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state())
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn post_json(body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/images/edits")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = build_router(state())
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).expect("序列化")))
                .expect("request"),
        )
        .await
        .expect("响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn requires_auth() {
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "make it blue".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_api_key", "{body}");
}

#[tokio::test]
async fn rejects_unsupported_content_type() {
    let response = build_router(state())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header("content-type", "text/plain")
                .header("authorization", format!("Bearer {AUTH_KEY}"))
                .body(Body::from("hello"))
                .expect("request"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_missing_prompt() {
    let (status, body) =
        post_multipart(vec![Part::File("image", "a.png", tiny_png())], Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "prompt", "{body}");
}

#[tokio::test]
async fn rejects_missing_image() {
    let (status, body) = post_multipart(
        vec![Part::Text("prompt", "make it blue".to_string())],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("image"),
        "{body}"
    );
}

#[tokio::test]
async fn rejects_n_out_of_range() {
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "x".to_string()),
            Part::Text("n", "9".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "n", "{body}");
}

#[tokio::test]
async fn rejects_stream() {
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "x".to_string()),
            Part::Text("stream", "true".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "stream", "{body}");
}

#[tokio::test]
async fn rejects_bad_response_format() {
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "x".to_string()),
            Part::Text("response_format", "png".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "response_format", "{body}");
}

#[tokio::test]
async fn multipart_valid_body_reaches_account_pool() {
    // 解析与校验都通过后，才会去取号；池空  503 no_available_account。
    // 这一步证明 multipart 解析真的走通了。
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "make it blue".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account", "{body}");
}

#[tokio::test]
async fn json_body_with_data_url_reaches_account_pool() {
    let body = serde_json::json!({
        "prompt": "make it blue",
        "image": format!(
            "data:image/png;base64,{}",
            {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(tiny_png())
            }
        ),
    });
    let (status, body) = post_json(body, Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "no_available_account", "{body}");
}

#[tokio::test]
async fn json_body_rejects_file_id_reference() {
    let body = serde_json::json!({
        "prompt": "make it blue",
        "images": [{ "file_id": "file-123" }],
    });
    let (status, body) = post_json(body, Some(AUTH_KEY)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("file_id"),
        "{body}"
    );
}

#[tokio::test]
async fn json_body_with_multiple_image_fields() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(tiny_png());
    let body = serde_json::json!({
        "prompt": "combine",
        "images": [
            format!("data:image/png;base64,{b64}"),
            { "b64_json": b64, "filename": "b.png" }
        ],
    });
    let (status, body) = post_json(body, Some(AUTH_KEY)).await;
    // 两张图都解析成功  走到取号
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn n_greater_than_one_reaches_account_pool() {
    // n=3 会走「并行 3 个槽位」的路径；池空时仍应是 503，而不是 500/502
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "a cat".to_string()),
            Part::Text("n", "3".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error"]["code"], "no_available_account",
        "池空必须报 no_available_account，不能报上游错误：{body}"
    );
}

#[tokio::test]
async fn rejects_bad_output_format() {
    let (status, body) = post_multipart(
        vec![
            Part::Text("prompt", "make it warm".to_string()),
            Part::Text("output_format", "tiff".to_string()),
            Part::File("image", "a.png", tiny_png()),
        ],
        Some(AUTH_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "不支持的输出格式必须拒绝");
    assert_eq!(body["error"]["param"], "output_format");
}

//! 缩略图：懒生成、缓存复用、删除、路径校验，以及路由层。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const TEST_KEY: &str = "test-admin-key-123456";

fn router_state() -> Arc<AppState> {
    let map: HashMap<&'static str, &'static str> = HashMap::from([
        (
            "DATABASE_URL",
            "postgres://nobody:nobody@127.0.0.1:1/nodb?sslmode=disable",
        ),
        ("ADMIN_KEY", TEST_KEY),
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
        Arc::new(gpt2apirs_account::AccountPool::new(
            gpt2apirs_account::AccountRepo::new(pool.clone()),
            gpt2apirs_account::PoolConfig::default(),
        )),
        Arc::new(gpt2apirs_account::ProgressStore::new()),
        Arc::new(gpt2apirs_account::OAuthSessions::new()),
        Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Memory(
            gpt2apirs_storage::repo::MemoryApiKeys::new().with_key(TEST_KEY, "test"),
        )),
    ))
}

/// 造一张 1200x800 的真 PNG 落到 `$DATA_DIR/images/...`，返回相对路径。
fn seed_image(dir: &std::path::Path) -> String {
    let rel = "images/2026/11/17/thumb-sample.png";
    let absolute = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(absolute.parent().expect("父目录")).expect("建目录");
    let image = image::RgbaImage::from_fn(1200, 800, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    });
    image::DynamicImage::ImageRgba8(image)
        .save(&absolute)
        .expect("写 PNG");
    rel.to_string()
}

fn temp_data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gpt2apirs-thumb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn generates_caches_and_removes() {
    let dir = temp_data_dir("cache");
    let rel = seed_image(&dir);

    let first = gpt2apirs_api::thumbnails::ensure_in(&dir, &rel)
        .expect("生成")
        .expect("有缩略图");
    assert!(first.is_file(), "缩略图应落盘");
    let thumb_rel = gpt2apirs_api::thumbnails::thumb_rel(&rel).expect("路径");
    assert!(thumb_rel.starts_with("thumbnails/images/"), "{thumb_rel}");
    assert!(thumb_rel.ends_with(".jpg"), "{thumb_rel}");

    // 长边压到 480，宽高比保持 3:2
    let decoded = image::open(&first).expect("解码缩略图");
    assert_eq!(decoded.width(), 480);
    assert_eq!(decoded.height(), 320);

    // 第二次直接命中缓存：路径一样、字节一样
    let cached = std::fs::read(&first).expect("读缓存");
    let second = gpt2apirs_api::thumbnails::ensure_in(&dir, &rel)
        .expect("再取")
        .expect("有缩略图");
    assert_eq!(first, second);
    assert_eq!(cached, std::fs::read(&second).expect("读缓存"));

    // 删除后可以重建
    gpt2apirs_api::thumbnails::remove_in(&dir, &rel);
    assert!(!first.exists(), "remove 应删掉缓存");
    assert!(gpt2apirs_api::thumbnails::ensure_in(&dir, &rel)
        .expect("重建")
        .is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_source_yields_none() {
    let dir = temp_data_dir("missing");
    let rel = "images/2026/11/17/nope.png";
    assert!(gpt2apirs_api::thumbnails::ensure_in(&dir, rel).expect("查询").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_traversal_and_foreign_paths() {
    assert!(!gpt2apirs_api::thumbnails::is_allowed_source(
        "images/../../etc/passwd"
    ));
    assert!(!gpt2apirs_api::thumbnails::is_allowed_source(
        "references/x.png"
    ));
    assert!(!gpt2apirs_api::thumbnails::is_allowed_source("images/x.txt"));
    assert!(gpt2apirs_api::thumbnails::is_allowed_source(
        "images/2026/11/17/a.png"
    ));
    assert!(gpt2apirs_api::thumbnails::thumb_rel("images/../../x.png").is_none());
}

#[tokio::test]
async fn route_serves_jpeg_thumbnail() {
    let dir = temp_data_dir("route");
    let rel = seed_image(&dir);
    // 路由走进程级 data_dir，这里单独设置（其它用例用显式根目录，不读环境变量）。
    unsafe {
        std::env::set_var("DATA_DIR", &dir);
    }
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/image-thumbnails/{}",
            rel.trim_start_matches("images/")
        ))
        .body(Body::empty())
        .expect("request");
    let response = build_router(router_state())
        .oneshot(request)
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(content_type, "image/jpeg");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn route_rejects_traversal() {
    let dir = temp_data_dir("route-bad");
    let request = Request::builder()
        .method("GET")
        .uri("/image-thumbnails/..%2F..%2Fetc%2Fpasswd")
        .body(Body::empty())
        .expect("request");
    let response = build_router(router_state())
        .oneshot(request)
        .await
        .expect("响应");
    assert_ne!(response.status(), StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

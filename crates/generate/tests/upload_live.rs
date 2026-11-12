//! 上游图片上传的真机测试。
//!
//! 需要 .env 里的 DATABASE_URL，且库里至少有一个 access_token 有效的账号；
//! 缺任一条件自动跳过。
//!
//! **上传不消耗图片配额** —— 它只是把文件放进上游存储，所以限流号也能用。
//!
//! 需要 `tls` feature：未启用时 `Client` 是存根，任何网络调用都会失败，
//! 那样的「失败」没有诊断价值，所以整个文件在无 tls 时直接不编译进去。

#![cfg(feature = "tls")]

use gpt2apirs_core::Deadline;
use gpt2apirs_http::{Client, ClientOptions, Emulation};
use gpt2apirs_storage::pool::build_pool;

fn prepare_env() -> bool {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        manifest.join("../../.env"),
        manifest.join("../../target/debug/.env"),
    ] {
        if path.is_file() {
            let _ = dotenvy::from_path(&path);
            break;
        }
    }
    std::env::var("DATABASE_URL")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// 造一张小 PNG（64x48）。
fn sample_png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(64, 48, image::Rgba([200, 30, 30, 255]));
    let mut buffer = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut buffer, image::ImageFormat::Png)
        .expect("编码 PNG");
    buffer.into_inner()
}

async fn pick_token(pool: &sqlx::PgPool) -> Option<(String, String)> {
    // 取任意一个有 access_token 的账号；上传不需要配额
    let row: Option<(uuid::Uuid, String, Option<String>)> = sqlx::query_as(
        "select id, access_token, email from accounts \
         where length(access_token) > 20 \
         order by (status = 'normal') desc, created_at desc limit 1",
    )
    .fetch_optional(pool)
    .await
    .ok()?;
    let (id, token, email) = row?;
    let label = email.unwrap_or_else(|| id.to_string());
    Some((token, label))
}

#[tokio::test]
async fn upload_png_reaches_upstream() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置应可加载");
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");

    let Some((token, label)) = pick_token(&pool).await else {
        eprintln!("跳过：库里没有可用账号");
        pool.close().await;
        return;
    };
    println!("使用账号: {label}");

    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: None,
        timeout_secs: Some(120),
    })
    .expect("客户端");

    let png = sample_png();
    println!("样本 PNG: {} 字节", png.len());

    let started = std::time::Instant::now();
    let uploaded = gpt2apirs_generate::upload::upload_image(
        &client,
        &token,
        gpt2apirs_http::DEFAULT_USER_AGENT,
        &png,
        Some("probe.png"),
        Deadline::in_secs(120.0),
    )
    .await
    .expect("上传应成功");
    println!(
        "上传成功 {:.1}s: file_id={} size={} mime={} {}x{}",
        started.elapsed().as_secs_f64(),
        uploaded.file_id,
        uploaded.file_size,
        uploaded.mime_type,
        uploaded.width,
        uploaded.height
    );

    assert!(!uploaded.file_id.is_empty(), "应返回 file_id");
    assert_eq!(uploaded.file_size, png.len(), "字节数应一致");
    assert_eq!(uploaded.mime_type, "image/png");
    assert_eq!(uploaded.width, 64);
    assert_eq!(uploaded.height, 48);
    assert_eq!(uploaded.file_name, "probe.png");

    pool.close().await;
}

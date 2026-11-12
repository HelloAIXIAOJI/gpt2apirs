//! 联网搜索的真机测试。
//!
//! 需要 .env 的 DATABASE_URL 与 token 有效的账号，缺任一条件自动跳过。
//! 会落一份脱敏后的会话详情到 `data/capture/11_search_detail.json` 供离线测试使用。

#![cfg(feature = "tls")]

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::conversation::fetch_conversation_payload;
use gpt2apirs_generate::search::search;
use gpt2apirs_http::{Client, ClientOptions, Emulation, DEFAULT_USER_AGENT};
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

async fn pick_token(pool: &sqlx::PgPool) -> Option<(String, String)> {
    let row: Option<(uuid::Uuid, String, Option<String>)> = sqlx::query_as(
        "select id, access_token, email from accounts \
         where length(access_token) > 20 \
         order by (status = 'normal') desc, created_at desc limit 1",
    )
    .fetch_optional(pool)
    .await
    .ok()?;
    let (id, token, email) = row?;
    Some((token, email.unwrap_or_else(|| id.to_string())))
}

#[tokio::test]
async fn search_returns_answer_and_sources() {
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
        timeout_secs: Some(300),
    })
    .expect("客户端");

    let started = std::time::Instant::now();
    let outcome = search(
        &client,
        &token,
        "2026 年 11 月有哪些值得关注的天文现象？",
        "gpt-5-5",
        Deadline::in_secs(300.0),
    )
    .await
    .expect("搜索应成功");

    println!("耗时 {:.1}s", started.elapsed().as_secs_f64());
    println!("conversation_id = {}", outcome.conversation_id);
    println!("状态 = {}", outcome.status);
    println!("答案（前 200 字）: {}", outcome.answer.chars().take(200).collect::<String>());
    println!("来源 {} 条", outcome.sources.len());
    for source in &outcome.sources {
        println!("  - {} | {}", source.title, source.url);
    }

    assert!(!outcome.answer.trim().is_empty(), "答案不应为空");
    assert!(!outcome.answer.contains('\u{E200}'), "注解必须被清掉");

    // 落原始详情，供离线解析测试
    let raw = fetch_conversation_payload(
        &client,
        &token,
        DEFAULT_USER_AGENT,
        &outcome.conversation_id,
        Deadline::in_secs(120.0),
    )
    .await
    .expect("应能取会话详情");
    let capture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/capture/11_search_detail_raw.json");
    if let Some(parent) = capture.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&capture, serde_json::to_vec_pretty(&raw).expect("序列化"))
        .expect("写样本");
    println!("原始详情 {} 字节", serde_json::to_string(&raw).map(|s| s.len()).unwrap_or(0));

    pool.close().await;
}

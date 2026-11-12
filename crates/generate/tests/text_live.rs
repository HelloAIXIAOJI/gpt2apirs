//! 文本链路的真机测试：抓真实 SSE，验证文本提取。
//!
//! 需要 .env 的 DATABASE_URL 与一个 token 有效的账号，缺任一条件自动跳过。
//! 会把原始 SSE 落到 `data/capture/08_text_sse_raw.txt` 供人工复核。

#![cfg(feature = "tls")]

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::models;
use gpt2apirs_generate::text::{TextRequest, generate_text};
use gpt2apirs_http::{Client, ClientOptions, Emulation};
use gpt2apirs_protocol::text_stream::collect_text;
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
async fn text_generation_returns_real_answer() {
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
        timeout_secs: Some(180),
    })
    .expect("客户端");

    let mut request = TextRequest::new("用一句话说明什么是 HTTP 状态码 404。不要用列表。");
    request.upstream_model = models::upstream_model_for("gpt-5-5").to_string();

    let started = std::time::Instant::now();
    let (result, diag) = generate_text(&client, &token, &request, Deadline::in_secs(180.0))
        .await
        .expect("文本生成应成功");
    println!("耗时 {:.1}s", started.elapsed().as_secs_f64());
    println!("conversation_id = {}", result.conversation_id);
    println!("SSE 字节 = {}", result.sse_raw.len());
    println!("增量片段 = {} 个", result.deltas.len());
    println!("--- 回复正文 ---");
    println!("{}", result.text);
    println!("--- 正文结束（{} 字符）---", result.text.chars().count());
    println!(
        "诊断: bootstrap={} prepare={} finalize={} pow={} turnstile={}",
        diag.bootstrap_status,
        diag.prepare_status,
        diag.finalize_status,
        diag.pow_required,
        diag.turnstile_required
    );

    let capture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/capture/08_text_sse_raw.txt");
    if let Some(parent) = capture.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&capture, &result.sse_raw).expect("写 SSE 样本");
    println!("SSE 已存: {}", capture.display());

    assert!(!result.text.trim().is_empty(), "回复不应为空");
    assert!(
        !result.text.contains('\u{E200}'),
        "内部注解标记不应出现在输出里"
    );
    let joined: String = result.deltas.concat();
    assert_eq!(joined, result.text, "增量拼接应等于全文");

    let (again, _, _) = collect_text(&result.sse_raw, "");
    assert_eq!(again, result.text, "重复解析应得到相同文本");

    pool.close().await;
}

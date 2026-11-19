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

/// 真机测试可选走代理：本机直连不通时设 `GPT2APIRS_TEST_PROXY=http://127.0.0.1:端口`。
fn test_proxy() -> Option<String> {
    std::env::var("GPT2APIRS_TEST_PROXY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

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

/// 拿一个可用 token：优先 `GPT2APIRS_TEST_TOKEN`，其次数据库。
///
/// 有环境变量时完全不碰数据库 —— 开 VPN 时数据库地址也会被带走，连不上。
async fn token_or_skip() -> Option<(String, Option<sqlx::PgPool>)> {
    if let Ok(token) = std::env::var("GPT2APIRS_TEST_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            println!("使用账号: <GPT2APIRS_TEST_TOKEN>");
            return Some((token, None));
        }
    }
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置应可加载");
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");
    let Some((token, label)) = pick_token(&pool).await else {
        eprintln!("跳过：库里没有可用账号");
        pool.close().await;
        return None;
    };
    println!("使用账号: {label}");
    Some((token, Some(pool)))
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
    let Some((token, pool)) = token_or_skip().await else {
        return;
    };

    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: test_proxy(),
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
    let joined = join_text(&result.deltas);
    assert_eq!(joined, result.text, "增量拼接应等于全文");

    // 思考内容必须与正文分开：正文里不该出现思考段的文本
    println!(
        "思考内容 {} 字节 / 正文 {} 字节",
        result.reasoning.len(),
        result.text.len()
    );
    if !result.reasoning.trim().is_empty() {
        assert!(
            !result.text.contains(result.reasoning.trim()),
            "思考内容不能混进正文"
        );
    }

    let (again, _reasoning, _cid, _deltas) = collect_text(&result.sse_raw, &[]);
    assert_eq!(again, result.text, "重复解析应得到相同文本");

    if let Some(pool) = pool {
        pool.close().await;
    }
}

/// 多轮对话：上一轮助手正文**不能**被当成这一轮的开头重发出去。
///
/// 上游在这一轮的首帧里会把上一轮的助手正文重新下发一次；
/// 累积器必须认得出「这不是本轮新生成的内容」，否则流式客户端会看到
/// 上一轮的话被原样重复一遍（非流式看不见，因为最终值被后续帧覆盖）。
#[tokio::test]
async fn multi_turn_does_not_repeat_previous_reply() {
    let Some((token, pool)) = token_or_skip().await else {
        return;
    };

    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: test_proxy(),
        timeout_secs: Some(180),
    })
    .expect("客户端");

    let messages = vec![
        ("user".to_string(), "你好，你叫什么名字？".to_string()),
        (
            "assistant".to_string(),
            "MARKER_PREV_REPLY_ALPHA 我叫小暖，很高兴认识你。".to_string(),
        ),
        ("user".to_string(), "再说一句，只回一句话。".to_string()),
    ];

    // 与生产调用点一致：只给 messages，不额外塞 hint
    let mut request = TextRequest::new(String::new()).with_messages(messages);
    request.upstream_model = models::upstream_model_for("gpt-5-5").to_string();

    let (result, _diag) = generate_text(&client, &token, &request, Deadline::in_secs(180.0))
        .await
        .expect("文本生成应成功");

    let capture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/capture/09_text_multiturn_sse_raw.txt");
    if let Some(parent) = capture.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&capture, &result.sse_raw).expect("写 SSE 样本");

    let joined = join_text(&result.deltas);
    println!("--- 最终正文 ---");
    println!("{}", result.text);
    println!("--- 增量拼接 ---");
    println!("{joined}");
    println!("SSE 已存: {}", capture.display());

    assert!(
        !result.text.contains("MARKER_PREV_REPLY_ALPHA"),
        "最终正文里出现了上一轮的助手原文：{}",
        result.text
    );
    assert!(
        !joined.contains("MARKER_PREV_REPLY_ALPHA"),
        "流式增量里出现了上一轮的助手原文：{joined}"
    );

    if let Some(pool) = pool {
        pool.close().await;
    }
}

/// 只拼正文增量（思考内容单列，不参与拼接）。
fn join_text(deltas: &[gpt2apirs_protocol::text_stream::Delta]) -> String {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            gpt2apirs_protocol::text_stream::Delta::Text(text) => Some(text.as_str()),
            gpt2apirs_protocol::text_stream::Delta::Reasoning(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 思考强度透传：上游接受就带上，拒绝（422）就自动摘掉重发，不能让请求整体失败。
#[tokio::test]
async fn thinking_effort_is_accepted_by_upstream() {
    let Some((token, pool)) = token_or_skip().await else {
        return;
    };

    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: test_proxy(),
        timeout_secs: Some(180),
    })
    .expect("客户端");

    let mut request = TextRequest::new("用一句话说明什么是二分查找。不要用列表。")
        .with_thinking_effort(Some("low".to_string()));
    request.upstream_model = models::upstream_model_for("gpt-5-5").to_string();

    let started = std::time::Instant::now();
    let (result, _diag) = generate_text(&client, &token, &request, Deadline::in_secs(180.0))
        .await
        .expect("带 thinking_effort 的请求应被上游接受");
    println!(
        "耗时 {:.1}s / 正文 {} 字节 / 思考 {} 字节",
        started.elapsed().as_secs_f64(),
        result.text.len(),
        result.reasoning.len()
    );
    assert!(!result.text.trim().is_empty(), "回复不应为空");

    if let Some(pool) = pool {
        pool.close().await;
    }
}

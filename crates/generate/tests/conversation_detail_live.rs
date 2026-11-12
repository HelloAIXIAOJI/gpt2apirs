//! 会话详情的真机测试。
//!
//! 用**图片链路**产生的会话：文本链路带 `history_and_training_disabled: true`，
//! 上游不落库，GET 详情必然 404 —— 这不是实现问题，是那条链路的设计。
//!
//! 需要 .env 的 DATABASE_URL 与 token 有效的账号，缺任一条件自动跳过。
//! 会消耗一次生图配额，并把脱敏后的详情落到 `data/capture/10_conversation_detail.json`。

#![cfg(feature = "tls")]

use gpt2apirs_core::Deadline;
use gpt2apirs_generate::conversation::fetch_conversation_detail;
use gpt2apirs_generate::{ImageRequest, generate_images};
use gpt2apirs_http::{Client, ClientOptions, Emulation};
use gpt2apirs_protocol::conversation::latest_assistant_message;
use gpt2apirs_storage::pool::build_pool;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

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

// 会消耗一次生图配额，默认不跑：
//   cargo test -p gpt2apirs-generate --features tls --test conversation_detail_live -- --ignored --nocapture
#[ignore = "消耗一次生图配额"]
#[tokio::test]
async fn conversation_detail_round_trips() {
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

    // 1) 会话来源：优先复用 env 里的已有会话（免再花配额），否则真生成一次
    let conversation_id = match std::env::var("CONVERSATION_ID") {
        Ok(id) if !id.trim().is_empty() => {
            println!("复用已有会话: {id}");
            id.trim().to_string()
        }
        _ => {
            let request = ImageRequest::new("一个红色的圆点，纯色背景").with_n(1);
            let (result, _diag) =
                generate_images(&client, &token, &request, Deadline::in_secs(180.0))
                    .await
                    .expect("生图应成功");
            println!("conversation_id = {}", result.conversation_id);
            result.conversation_id
        }
    };
    assert!(!conversation_id.is_empty(), "应有会话 ID");

    // 2) 拉详情
    let detail = fetch_conversation_detail(
        &client,
        &token,
        UA,
        &conversation_id,
        Deadline::in_secs(120.0),
    )
    .await
    .expect("会话详情应能拉到");

    println!("会话标题: {:?}（{} 条消息）", detail.title, detail.messages.len());
    for message in &detail.messages {
        println!(
            "  [{}] {}{}",
            message.role,
            if message.hidden { "(隐藏) " } else { "" },
            message.text.chars().take(40).collect::<String>()
        );
    }

    assert_eq!(detail.id, conversation_id, "会话 ID 应一致");
    assert!(!detail.messages.is_empty(), "详情里应有消息");
    assert!(
        detail.messages.iter().any(|m| m.role == "user"),
        "应包含用户消息"
    );
    let assistant = latest_assistant_message(&detail).expect("应能取到最后一条可见助手消息");
    println!("助手正文前 60 字: {}", assistant.text.chars().take(60).collect::<String>());

    // 3) 抓原始 JSON：离线解析测试必须基于上游真实结构，不能用我们自己解析后的形状
    let raw_path = format!("{}{}", gpt2apirs_protocol::endpoints::BASE_URL,
        gpt2apirs_protocol::endpoints::conversation(&conversation_id));
    let mut raw_headers: Vec<(&'static str, String)> =
        gpt2apirs_http::api_headers(UA, &token, &gpt2apirs_protocol::endpoints::conversation(&conversation_id));
    raw_headers.retain(|(name, _)| !name.eq_ignore_ascii_case("referer"));
    raw_headers.push(("Referer", format!("{}/c/{conversation_id}", gpt2apirs_protocol::endpoints::BASE_URL)));
    let raw_response = client
        .get_text(&raw_path, &raw_headers, Some(std::time::Duration::from_secs(60)))
        .await
        .expect("原始详情请求");
    assert_eq!(raw_response.status, 200, "原始详情应 200");
    let raw_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/capture");
    let _ = std::fs::create_dir_all(&raw_dir);
    std::fs::write(raw_dir.join("10_conversation_detail_raw.json"), &raw_response.body)
        .expect("写原始样本");
    println!("原始样本 {} 字节", raw_response.body.len());

    // 4) 落脱敏样本（长文本截断，保留结构）
    let slim = serde_json::json!({
        "id": detail.id,
        "title": detail.title,
        "messages": detail.messages.iter().map(|m| serde_json::json!({
            "id": m.id,
            "role": m.role,
            "text": m.text.chars().take(200).collect::<String>(),
            "create_time": m.create_time,
            "status": m.status,
            "hidden": m.hidden,
        })).collect::<Vec<_>>(),
    });
    let capture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/capture/10_conversation_detail.json");
    if let Some(parent) = capture.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&capture, serde_json::to_vec_pretty(&slim).expect("序列化"))
        .expect("写样本");
    println!("样本已存: {}", capture.display());

    pool.close().await;
}

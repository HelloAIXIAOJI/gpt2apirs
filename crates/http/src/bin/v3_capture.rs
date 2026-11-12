//! V-3 采集工具：在你自己的机器上跑，抓取真实上游响应样本。
//!
//! - access_token 只从环境变量 `GPT2APIRS_ACCESS_TOKEN` 或 `--token-file` 读取；
//! - 本程序**从不**打印 token，输出里也不含 token；
//! - 采集结果写到 `data/capture/`（已在 .gitignore 中）；
//! - 建议使用**低价值备用账号**，不要用主账号。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gpt2apirs_protocol::{
    build_legacy_requirements_token, build_pow_config, build_proof_token, parse_pow_resources,
};
use serde_json::{Value, json};

const BASE: &str = "https://chatgpt.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";
const CLIENT_VERSION: &str = "prod-a194cd50d4416d3c0b47c740f206b12ce60f5887";
const CLIENT_BUILD_NUMBER: &str = "6708908";

/// 采集输出目录。由 `DATA_DIR` 决定（见 core::paths），**不依赖 CWD**。
fn capture_dir() -> Result<PathBuf> {
    Ok(gpt2apirs_core::paths::capture_dir()?)
}

fn main() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let token = read_token(&args)?;
    let token_len = token.len();
    println!("token 已读取（长度 {token_len}，内容不显示）");

    let cap_dir = capture_dir()?;
    println!("采集目录: {}", cap_dir.display());

    if is_placeholder_token(&token) {
        bail!("token 看起来是占位符（含 xxxxx/your-token/...），请填入真实 access_token");
    }

    let client = wreq::Client::builder()
        .emulation(wreq_util::Emulation::Chrome110)
        .timeout(Duration::from_secs(120))
        .build()
        .context("构建 wreq 客户端失败")?;

    // ---- 1. bootstrap：拿 PoW 脚本与 data-build ----
    println!("\n[1/5] GET / （bootstrap）");
    let home = client
        .get(format!("{BASE}/"))
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
        .context("bootstrap 请求失败")?;
    let home_status = home.status().as_u16();
    let html = home.text().await.unwrap_or_default();
    println!("  状态 {home_status}，HTML 长度 {}", html.len());
    if home_status != 200 {
        bail!("bootstrap 非 200，可能 token 失效或被 Cloudflare 拦截");
    }
    let (scripts, data_build) = parse_pow_resources(&html);
    println!("  script_sources = {}", scripts.len());
    println!("  data_build     = {data_build:?}");
    write_json("01_bootstrap.json", &json!({
        "status": home_status,
        "html_len": html.len(),
        "script_sources": scripts,
        "data_build": data_build,
    }))?;

    // ---- 2. sentinel prepare ----
    println!("\n[2/5] POST /backend-api/sentinel/chat-requirements/prepare");
    let config = build_pow_config(USER_AGENT, Some(&scripts), &data_build);
    let p_token = build_legacy_requirements_token(&config);
    let prepare_path = "/backend-api/sentinel/chat-requirements/prepare";
    let prepare_resp = client
        .post(format!("{BASE}{prepare_path}"))
        .headers(base_headers(&token, prepare_path))
        .header("Content-Type", "application/json")
        .json(&json!({ "p": p_token }))
        .send()
        .await
        .context("sentinel prepare 请求失败")?;
    let prepare_status = prepare_resp.status().as_u16();
    let prepare_text = prepare_resp.text().await.unwrap_or_default();
    println!("  状态 {prepare_status}");
    let prepare_json: Value = serde_json::from_str(&prepare_text).unwrap_or(Value::Null);
    write_json("02_sentinel_prepare.json", &json!({
        "status": prepare_status,
        "body": prepare_json,
        "raw": prepare_text.chars().take(4000).collect::<String>(),
    }))?;
    if prepare_status != 200 {
        println!("  ⚠ prepare 非 200，后续步骤大概率失败。原始响应前 300 字符：");
        println!("  {}", &prepare_text.chars().take(300).collect::<String>());
    }

    // ---- 3. sentinel finalize（含 PoW 求解）----
    println!("\n[3/5] POST /backend-api/sentinel/chat-requirements/finalize");
    let proof_info = prepare_json.get("proofofwork").cloned().unwrap_or(Value::Null);
    let proof_required = proof_info.get("required").and_then(Value::as_bool).unwrap_or(false);
    let proof_token = if proof_required {
        let seed = proof_info.get("seed").and_then(Value::as_str).unwrap_or("");
        let difficulty = proof_info.get("difficulty").and_then(Value::as_str).unwrap_or("");
        println!("  PoW 需要求解：difficulty={difficulty}");
        build_proof_token(seed, difficulty, &config).context("PoW 求解失败")?
    } else {
        println!("  PoW 未要求，跳过");
        String::new()
    };

    let prepare_token = prepare_json
        .get("prepare_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let finalize_path = "/backend-api/sentinel/chat-requirements/finalize";
    let finalize_resp = client
        .post(format!("{BASE}{finalize_path}"))
        .headers(base_headers(&token, finalize_path))
        .header("Content-Type", "application/json")
        .json(&json!({
            "prepare_token": prepare_token,
            "proof_token": proof_token,
            "turnstile_token": "",
        }))
        .send()
        .await
        .context("sentinel finalize 请求失败")?;
    let finalize_status = finalize_resp.status().as_u16();
    let finalize_text = finalize_resp.text().await.unwrap_or_default();
    println!("  状态 {finalize_status}");
    let finalize_json: Value = serde_json::from_str(&finalize_text).unwrap_or(Value::Null);
    write_json("03_sentinel_finalize.json", &json!({
        "status": finalize_status,
        "body": finalize_json,
        "raw": finalize_text.chars().take(4000).collect::<String>(),
    }))?;

    let Some(requirements_token) = finalize_json
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        bail!("finalize 未返回 token，停止。详情见 03_sentinel_finalize.json");
    };
    println!("  requirements token 已获取（长度 {}）", requirements_token.len());

    // ---- 4. 图片会话 prepare：拿 conduit_token ----
    println!("\n[4/5] POST /backend-api/f/conversation/prepare");
    let prompt = std::env::var("V3_PROMPT")
        .unwrap_or_else(|_| "a simple red apple on a white table".to_string());
    println!("  使用提示词（可用 V3_PROMPT 覆盖）: {prompt:?}");

    let prepare2_path = "/backend-api/f/conversation/prepare";
    let conv_prepare_payload = json!({
        "action": "next",
        "fork_from_shared_post": false,
        "parent_message_id": uuid_v4(),
        "model": "gpt-5-5",
        "client_prepare_state": "success",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "system_hints": ["picture_v2"],
        "partial_query": {
            "id": uuid_v4(),
            "author": { "role": "user" },
            "content": { "content_type": "text", "parts": [prompt] },
        },
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": { "app_name": "chatgpt.com" },
    });
    let conv_prepare_resp = client
        .post(format!("{BASE}{prepare2_path}"))
        .headers(image_headers(
            &token,
            prepare2_path,
            Some(&requirements_token),
            Some(&proof_token),
            None,
            None,
        ))
        .header("Content-Type", "application/json")
        .json(&conv_prepare_payload)
        .send()
        .await
        .context("conversation/prepare 请求失败")?;
    let conv_prepare_status = conv_prepare_resp.status().as_u16();
    let conv_prepare_text = conv_prepare_resp.text().await.unwrap_or_default();
    println!("  状态 {conv_prepare_status}");
    let conv_prepare_json: Value =
        serde_json::from_str(&conv_prepare_text).unwrap_or(Value::Null);
    write_json("04_conversation_prepare.json", &json!({
        "status": conv_prepare_status,
        "body": conv_prepare_json,
        "raw": conv_prepare_text.chars().take(4000).collect::<String>(),
    }))?;

    let Some(conduit_token) = conv_prepare_json
        .get("conduit_token")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        bail!("prepare 未返回 conduit_token，停止。详情见 04_conversation_prepare.json");
    };
    println!("  conduit_token 已获取（长度 {}）", conduit_token.len());

    // ---- 5. 发起生图 SSE 并完整捕获 ----
    println!("\n[5/5] POST /backend-api/f/conversation （SSE 捕获）");
    let gen_path = "/backend-api/f/conversation";
    let gen_payload = json!({
        "action": "next",
        "messages": [{
            "id": uuid_v4(),
            "author": { "role": "user" },
            "create_time": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            "content": { "content_type": "text", "parts": [prompt] },
            "metadata": {
                "developer_mode_connector_ids": [],
                "selected_github_repos": [],
                "selected_all_github_repos": false,
                "system_hints": ["picture_v2"],
                "serialization_metadata": { "custom_symbol_offsets": [] },
            },
        }],
        "parent_message_id": uuid_v4(),
        "model": "gpt-5-5",
        "client_prepare_state": "sent",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "enable_message_followups": true,
        "system_hints": ["picture_v2"],
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": {
            "is_dark_mode": false,
            "time_since_loaded": 1200,
            "page_height": 1072,
            "page_width": 1724,
            "pixel_ratio": 1.2,
            "screen_height": 1440,
            "screen_width": 2560,
            "app_name": "chatgpt.com",
        },
        "paragen_cot_summary_display_override": "allow",
        "force_parallel_switch": "auto",
    });

    let gen_resp = client
        .post(format!("{BASE}{gen_path}"))
        .headers(image_headers(
            &token,
            gen_path,
            Some(&requirements_token),
            Some(&proof_token),
            Some(&conduit_token),
            Some("text/event-stream"),
        ))
        .header("Content-Type", "application/json")
        .json(&gen_payload)
        .send()
        .await
        .context("生图 SSE 请求失败")?;
    let gen_status = gen_resp.status().as_u16();
    println!("  状态 {gen_status}");
    if gen_status != 200 {
        let body = gen_resp.text().await.unwrap_or_default();
        write_text("05_conversation_error.txt", &body)?;
        bail!("生图请求非 200，原始响应已存 05_conversation_error.txt");
    }

    // 逐块读取并落盘，同时统计事件类型
    let mut sse = String::new();
    let mut chunk_count = 0usize;
    let mut stream = gen_resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("读取 SSE 分块失败")?;
        chunk_count += 1;
        sse.push_str(&String::from_utf8_lossy(&bytes));
        if sse.len() > 8 * 1024 * 1024 {
            println!("  ⚠ 已捕获 8 MB，停止读取");
            break;
        }
    }
    println!("  收到 {chunk_count} 个分块，累计 {} 字节", sse.len());

    // 统计 data: 行与事件类型（不含内容）
    let mut data_lines = 0usize;
    let mut event_types: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for line in sse.lines() {
        let Some(rest) = line.strip_prefix("data: ") else {
            continue;
        };
        data_lines += 1;
        if rest == "[DONE]" {
            *event_types.entry("[DONE]".to_string()).or_insert(0) += 1;
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(rest) {
            let key = value
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| value.get("event").and_then(Value::as_str))
                .unwrap_or("<none>")
                .to_string();
            *event_types.entry(key).or_insert(0) += 1;
        }
    }
    println!("  data: 行数 = {data_lines}");
    println!("  事件类型分布:");
    for (name, count) in &event_types {
        println!("    {name:32} {count}");
    }

    write_text("06_sse_raw.txt", &sse)?;
    write_json("07_sse_summary.json", &json!({
        "status": gen_status,
        "chunk_count": chunk_count,
        "total_bytes": sse.len(),
        "data_lines": data_lines,
        "event_types": event_types,
    }))?;

    println!("\n完成。产出目录 {}：", cap_dir.display());
    println!("  01_bootstrap.json");
    println!("  02_sentinel_prepare.json");
    println!("  03_sentinel_finalize.json");
    println!("  04_conversation_prepare.json");
    println!("  06_sse_raw.txt          ← 最关键，SSE 原始字节");
    println!("  07_sse_summary.json");

    Ok(())
}

// 辅助函数

/// 读取 token：优先 `--token-file`，其次环境变量。
fn read_token(args: &[String]) -> Result<String> {
    if let Some(pos) = args.iter().position(|a| a == "--token-file") {
        let path = args.get(pos + 1).context("--token-file 后需跟路径")?;
        let raw = std::fs::read_to_string(Path::new(path))
            .with_context(|| format!("读取 token 文件失败: {path}"))?;
        return Ok(raw.trim().to_string());
    }
    std::env::var("GPT2APIRS_ACCESS_TOKEN")
        .map(|v| v.trim().to_string())
        .context("未提供 token：请设置 GPT2APIRS_ACCESS_TOKEN 或传 --token-file")
}

/// 粗判占位符，避免把示例 token 当真。
fn is_placeholder_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("xxxx")
        || lower.contains("your-token")
        || lower.contains("eyjhbGci...")
        || token.len() < 100
}

/// 通用请求头（含伪造浏览器标识）。
fn base_headers(token: &str, path: &str) -> wreq::header::HeaderMap {
    let mut headers = wreq::header::HeaderMap::new();
    let pairs: Vec<(&str, String)> = vec![
        ("User-Agent", USER_AGENT.to_string()),
        ("Accept", "*/*".to_string()),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8,en-US;q=0.7".to_string()),
        ("Authorization", format!("Bearer {token}")),
        ("Origin", BASE.to_string()),
        ("Referer", format!("{BASE}/")),
        ("Cache-Control", "no-cache".to_string()),
        ("Pragma", "no-cache".to_string()),
        ("Priority", "u=1, i".to_string()),
        ("Sec-Ch-Ua", "\"Microsoft Edge\";v=\"143\", \"Chromium\";v=\"143\", \"Not A(Brand\";v=\"24\"".to_string()),
        ("Sec-Ch-Ua-Arch", "\"x86\"".to_string()),
        ("Sec-Ch-Ua-Bitness", "\"64\"".to_string()),
        ("Sec-Ch-Ua-Full-Version", "\"143.0.3650.96\"".to_string()),
        ("Sec-Ch-Ua-Mobile", "?0".to_string()),
        ("Sec-Ch-Ua-Model", "\"\"".to_string()),
        ("Sec-Ch-Ua-Platform", "\"Windows\"".to_string()),
        ("Sec-Ch-Ua-Platform-Version", "\"19.0.0\"".to_string()),
        ("Sec-Fetch-Dest", "empty".to_string()),
        ("Sec-Fetch-Mode", "cors".to_string()),
        ("Sec-Fetch-Site", "same-origin".to_string()),
        ("OAI-Device-Id", uuid_v4()),
        ("OAI-Session-Id", uuid_v4()),
        ("OAI-Language", "zh-CN".to_string()),
        ("OAI-Client-Version", CLIENT_VERSION.to_string()),
        ("OAI-Client-Build-Number", CLIENT_BUILD_NUMBER.to_string()),
        ("X-OpenAI-Target-Path", path.to_string()),
        ("X-OpenAI-Target-Route", path.to_string()),
    ];
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            wreq::header::HeaderName::from_bytes(name.as_bytes()),
            wreq::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// 图片链路请求头：额外带 Sentinel 与 conduit 凭据。
fn image_headers(
    token: &str,
    path: &str,
    requirements_token: Option<&str>,
    proof_token: Option<&str>,
    conduit_token: Option<&str>,
    accept: Option<&str>,
) -> wreq::header::HeaderMap {
    let mut headers = base_headers(token, path);
    let mut put = |name: &str, value: &str| {
        if let (Ok(n), Ok(v)) = (
            wreq::header::HeaderName::from_bytes(name.as_bytes()),
            wreq::header::HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    };
    if let Some(accept) = accept {
        put("Accept", accept);
    }
    if let Some(value) = requirements_token {
        put("OpenAI-Sentinel-Chat-Requirements-Token", value);
    }
    if let Some(value) = proof_token.filter(|v| !v.is_empty()) {
        put("OpenAI-Sentinel-Proof-Token", value);
    }
    if let Some(value) = conduit_token {
        put("X-Conduit-Token", value);
    }
    if accept == Some("text/event-stream") {
        put("X-Oai-Turn-Trace-Id", &uuid_v4());
    }
    headers
}

/// 生成 uuid v4 字符串。
fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn write_json(name: &str, value: &Value) -> Result<()> {
    let path = capture_dir()?.join(name);
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(&path, text).with_context(|| format!("写入 {name} 失败"))?;
    println!("  → 已写入 {name}");
    Ok(())
}

fn write_text(name: &str, value: &str) -> Result<()> {
    let path = capture_dir()?.join(name);
    std::fs::write(&path, value).with_context(|| format!("写入 {name} 失败"))?;
    Ok(())
}

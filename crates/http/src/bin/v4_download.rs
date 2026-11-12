//! V-4 图片下载工具：把 sediment/file ID 换成真实图片。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gpt2apirs_protocol::{
    attachment_download_path, file_download_path, is_same_origin, parse_conversation_stream,
    parse_download_url, plan_download_targets,
};

const BASE: &str = "https://chatgpt.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";

fn main() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let token = read_token(&args)?;
    println!("token 已读取（长度 {}，内容不显示）", token.len());
    if is_placeholder_token(&token) {
        bail!("token 看起来是占位符，请填入真实 access_token");
    }

    // 默认路径由 DATA_DIR 决定，不依赖 CWD
    let cap_dir = gpt2apirs_core::paths::capture_dir()?;
    let sse_path = arg_value(&args, "--sse")
        .map(PathBuf::from)
        .unwrap_or_else(|| cap_dir.join("06_sse_raw.txt"));
    let out_path = arg_value(&args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| cap_dir.join("image.png"));

    println!("SSE 输入: {}", sse_path.display());
    let raw = std::fs::read_to_string(&sse_path)
        .with_context(|| format!("读取 SSE 文件失败: {}", sse_path.display()))?;

    // ---- 1. 解析 SSE，拿 ID ----
    let (_, state) = parse_conversation_stream(&raw);
    println!("\n=== 1. 从 SSE 解析出的 ID ===");
    println!("  conversation_id = {}", if state.conversation_id.is_empty() { "<空>" } else { &state.conversation_id });
    println!("  file_ids        = {:?}", state.file_ids);
    println!("  sediment_ids    = {:?}", state.sediment_ids);

    if state.conversation_id.is_empty() && state.file_ids.is_empty() && state.sediment_ids.is_empty() {
        bail!("SSE 里没有可用的 ID，无法下载");
    }

    let (attachments, files) =
        plan_download_targets(&state.conversation_id, &state.file_ids, &state.sediment_ids);
    println!("  待解析 attachment: {} 个", attachments.len());
    println!("  待解析 file      : {} 个", files.len());

    // ---- 2. 解析下载 URL ----
    let client = wreq::Client::builder()
        .emulation(wreq_util::Emulation::Chrome110)
        .timeout(Duration::from_secs(120))
        .build()
        .context("构建 wreq 客户端失败")?;

    println!("\n=== 2. 解析下载 URL ===");
    let mut download_urls: Vec<String> = Vec::new();

    for (conversation_id, sediment_id) in &attachments {
        let path = attachment_download_path(conversation_id, sediment_id);
        println!("  attachment: {sediment_id}");
        match resolve_url(&client, &token, &path).await {
            Ok(Some(url)) => {
                println!("    -> {}", shorten(&url));
                if !download_urls.contains(&url) {
                    download_urls.push(url);
                }
            }
            Ok(None) => println!("    -> （响应里没有 download_url/url）"),
            Err(err) => println!("    -> 解析失败: {err}"),
        }
    }

    for file_id in &files {
        let path = file_download_path(file_id);
        println!("  file: {file_id}");
        match resolve_url(&client, &token, &path).await {
            Ok(Some(url)) => {
                println!("    -> {}", shorten(&url));
                if !download_urls.contains(&url) {
                    download_urls.push(url);
                }
            }
            Ok(None) => println!("    -> （响应里没有 download_url/url）"),
            Err(err) => println!("    -> 解析失败: {err}"),
        }
    }

    if download_urls.is_empty() {
        bail!("没有解析出任何下载 URL");
    }

    // ---- 3. 下载图片 ----
    println!("\n=== 3. 下载图片 ===");
    let mut saved = 0usize;
    for (index, url) in download_urls.iter().enumerate() {
        let same_origin = is_same_origin(BASE, url);
        println!(
            "  [{index}] {} (同源={})",
            shorten(url),
            same_origin
        );

        let mut request = client.get(url.clone()).header("User-Agent", USER_AGENT);
        if same_origin {
            // 仅同源才带账号凭据
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request.send().await.context("下载请求失败")?;
        let status = response.status().as_u16();
        if status != 200 {
            println!("    -> 状态 {status}，跳过");
            continue;
        }
        let bytes = response.bytes().await.context("读取图片字节失败")?;
        let kind = sniff_image(&bytes);
        println!(
            "    -> {} 字节，格式={}",
            bytes.len(),
            kind.unwrap_or("未知")
        );

        if kind.is_none() {
            println!("    -> 不是可识别的图片，跳过");
            continue;
        }

        let target = if saved == 0 {
            out_path.clone()
        } else {
            let stem = out_path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
            let ext = out_path.extension().and_then(|s| s.to_str()).unwrap_or("png");
            out_path.with_file_name(format!("{stem}_{saved}.{ext}"))
        };
        std::fs::write(&target, &bytes)
            .with_context(|| format!("写入 {} 失败", target.display()))?;
        println!("    -> 已保存 {}", target.display());
        saved += 1;
    }

    if saved == 0 {
        bail!("没有成功下载任何图片");
    }
    println!("\n完成，共保存 {saved} 张图片。");
    Ok(())
}

/// 请求下载地址解析接口，取出 URL。
async fn resolve_url(
    client: &wreq::Client,
    token: &str,
    path: &str,
) -> Result<Option<String>> {
    let response = client
        .get(format!("{BASE}{path}"))
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("Origin", BASE)
        .header("Referer", format!("{BASE}/"))
        .header("X-OpenAI-Target-Path", path)
        .header("X-OpenAI-Target-Route", path)
        .send()
        .await
        .context("下载地址解析请求失败")?;

    let status = response.status().as_u16();
    if status != 200 {
        let body = response.text().await.unwrap_or_default();
        bail!("状态 {status}: {}", shorten(&body));
    }
    let body: serde_json::Value = response.json().await.context("响应不是 JSON")?;
    Ok(parse_download_url(&body))
}

/// 识别图片格式（返回扩展名）。
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpeg");
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("gif");
    }
    None
}

fn shorten(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() > 120 {
        let head: String = one_line.chars().take(120).collect();
        format!("{head}…")
    } else {
        one_line
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|pos| args.get(pos + 1))
        .cloned()
}

fn read_token(args: &[String]) -> Result<String> {
    if let Some(path) = arg_value(args, "--token-file") {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 token 文件失败: {path}"))?;
        return Ok(raw.trim().to_string());
    }
    std::env::var("GPT2APIRS_ACCESS_TOKEN")
        .map(|v| v.trim().to_string())
        .context("未提供 token：请设置 GPT2APIRS_ACCESS_TOKEN 或传 --token-file")
}

fn is_placeholder_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("xxxx")
        || lower.contains("your-token")
        || lower.contains("eyjhbGci...")
        || token.len() < 100
}

//! 一条命令完成生图：bootstrap -> sentinel -> SSE -> 下载。
//!
//! ```bash
//! cargo run --release -p gpt2apirs-generate --features tls --bin generate -- \\
//!     --token-file "$env:USERPROFILE\\.gpt2apirs\\token.txt" \\
//!     --prompt "a simple red apple on a white table" \\
//!     --out data/capture/generated.png
//! ```
//!
//! token 只从文件/环境变量读取，从不打印。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use gpt2apirs_core::Deadline;
use gpt2apirs_generate::{ImageRequest, generate_images};
use gpt2apirs_http::{Client, ClientOptions};

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
    if is_placeholder(&token) {
        bail!("token 看起来是占位符，请填入真实 access_token");
    }

    let prompt = arg_value(&args, "--prompt")
        .unwrap_or_else(|| "a simple red apple on a white table".to_string());
    // 默认输出目录由 DATA_DIR 决定，不依赖 CWD
    let cap_dir = gpt2apirs_core::paths::capture_dir()?;
    let out = arg_value(&args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| cap_dir.join("generated.png"));
    println!("提示词: {prompt}");
    println!("输出: {}", out.display());

    let client = Client::new(ClientOptions::default()).context("构建客户端失败")?;
    let request = ImageRequest::new(prompt);

    println!("\n开始生成...");
    // 命令行工具用 170s 总时限
    let deadline = Deadline::in_secs(170.0);
    let (result, diag) = generate_images(&client, &token, &request, deadline).await?;

    println!("\n=== 各阶段诊断 ===");
    println!("  bootstrap          : {} (HTML {} 字节, {} 个脚本)", diag.bootstrap_status, diag.html_len, diag.script_count);
    println!("  data-build         : {}", if diag.data_build.is_empty() { "<空>" } else { &diag.data_build });
    println!("  sentinel prepare   : {}", diag.prepare_status);
    println!("  PoW 要求           : {} (difficulty={})", diag.pow_required, if diag.pow_difficulty.is_empty() { "-" } else { &diag.pow_difficulty });
    println!("  sentinel finalize  : {}", diag.finalize_status);
    println!("  conversation/prep  : {}", diag.conv_prepare_status);
    println!("  SSE                : {} ({} 字节)", diag.sse_status, diag.sse_bytes);

    println!("\n=== 解析结果 ===");
    println!("  conversation_id : {}", if result.conversation_id.is_empty() { "<空>" } else { &result.conversation_id });
    println!("  sediment_ids    : {:?}", result.sediment_ids);
    println!("  file_ids        : {:?}", result.file_ids);
    println!("  图片数量        : {}", result.images.len());

    if result.images.is_empty() {
        // 保存 SSE 便于排查
        let sse_path = cap_dir.join("last_failed_sse.txt");
        if let Some(parent) = sse_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&sse_path, &result.sse_raw).ok();
        bail!("没有下载到图片。原始 SSE 已存 {}", sse_path.display());
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // 注：图片已由 save_image 落到 DATA_DIR/images/YYYY/MM/DD/，
    // 这里只是按 --out 再复制一份方便查看。
    for (index, image) in result.images.iter().enumerate() {
        let target = if index == 0 {
            out.clone()
        } else {
            let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
            let ext = out.extension().and_then(|s| s.to_str()).unwrap_or("png");
            out.with_file_name(format!("{stem}_{index}.{ext}"))
        };
        std::fs::write(&target, &image.bytes)
            .with_context(|| format!("写入 {} 失败", target.display()))?;
        println!(
            "\n已保存 {}（{} 字节，{} {}x{}）\n  服务内路径: {}",
            target.display(),
            image.bytes.len(),
            image.mime,
            image.width,
            image.height,
            image.rel_path
        );
    }

    println!("\n完成。");
    Ok(())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|p| args.get(p + 1)).cloned()
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

fn is_placeholder(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("xxxx") || lower.contains("your-token") || lower.contains("eyjhbGci...") || token.len() < 100
}

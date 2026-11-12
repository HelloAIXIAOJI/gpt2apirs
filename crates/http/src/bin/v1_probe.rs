//! V-1 技术验证：wreq 能否通过 ChatGPT 的 Cloudflare。

use std::time::Duration;

const TARGETS: &[(&str, &str)] = &[
    ("首页", "https://chatgpt.com/"),
    ("匿名模型列表", "https://chatgpt.com/backend-anon/models"),
    ("鉴权账号信息", "https://chatgpt.com/backend-api/me"),
];

fn classify(body: &str) -> &'static str {
    let lower = body.to_ascii_lowercase();
    if lower.contains("just a moment") || lower.contains("cf-chl") || lower.contains("challenge-platform") {
        "CLOUDFLARE_BLOCKED"
    } else if body.trim_start().starts_with('{') {
        "OPENAI_JSON"
    } else if lower.contains("<html") {
        "HTML_OTHER"
    } else {
        "UNKNOWN"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let token = std::env::var("GPT2APIRS_ACCESS_TOKEN").ok();
    let attempts: u32 = std::env::var("V1_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let client = wreq::Client::builder()
        .emulation(wreq_util::Emulation::Chrome110)
        .timeout(Duration::from_secs(30))
        .build()?;

    tracing::info!(attempts, has_token = token.is_some(), "V-1 开始");

    let mut cloudflare_blocked = 0u32;
    let mut total = 0u32;

    for round in 1..=attempts {
        for (name, url) in TARGETS {
            total += 1;
            let mut req = client
                .get(*url)
                .header("Accept", "*/*")
                .header("Referer", "https://chatgpt.com/");
            if let Some(t) = token.as_deref() {
                req = req.header("Authorization", format!("Bearer {t}"));
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    let kind = classify(&body);
                    let preview: String = body.chars().take(120).collect();
                    if kind == "CLOUDFLARE_BLOCKED" {
                        cloudflare_blocked += 1;
                        tracing::error!(round, name, status, kind, body = %preview, "被 Cloudflare 拦截");
                    } else {
                        tracing::info!(round, name, status, kind, body = %preview, "通过");
                    }
                }
                Err(err) => {
                    cloudflare_blocked += 1;
                    tracing::error!(round, name, error = %err, "请求失败");
                }
            }
        }
    }

    tracing::info!(total, cloudflare_blocked, "V-1 结果");
    if cloudflare_blocked == 0 {
        tracing::info!("V-1 通过：wreq 完成 TLS 握手，Cloudflare 未拦截");
        Ok(())
    } else {
        anyhow::bail!("V-1 失败：{cloudflare_blocked}/{total} 次被拦截或失败")
    }
}

//! 探测上游模型列表端点（用号池里的真实账号）。
//!
//! 只读：模型列表不消耗配额。

#![cfg(feature = "tls")]

use gpt2apirs_http::{Client, ClientOptions, Emulation};
use gpt2apirs_protocol::endpoints::{BASE_URL_ENV, MODELS, MODELS_ANON, url as upstream_url};
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

async fn pick_token(pool: &sqlx::PgPool) -> Option<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "select access_token from accounts where length(access_token) > 20 \
         order by (status = 'normal') desc, created_at desc limit 1",
    )
    .fetch_optional(pool)
    .await
    .ok()?;
    row.map(|(token,)| token)
}

async fn probe(client: &Client, token: Option<&str>, path: &str) {
    let ua = gpt2apirs_generate::DEFAULT_USER_AGENT;
    let headers = match token {
        Some(value) => gpt2apirs_http::api_headers(ua, value, path),
        None => gpt2apirs_http::browser_headers(ua),
    };
    let target = upstream_url(path);
    match client
        .get_text(&target, &headers, Some(std::time::Duration::from_secs(30)))
        .await
    {
        Ok(response) => {
            let ids = extract_ids(&response.body);
            println!(
                "PROBE path={path} status={} bytes={} ids={:?}",
                response.status,
                response.body.len(),
                ids
            );
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&response.body) {
                if let Some(items) = value.get("models").and_then(|v| v.as_array()) {
                    for item in items {
                        println!(
                            "MODEL slug={} title={} max_tokens={}",
                            item.get("slug").and_then(|v| v.as_str()).unwrap_or("-"),
                            item.get("title").and_then(|v| v.as_str()).unwrap_or("-"),
                            item.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                        );
                    }
                }
            }
        }
        Err(error) => println!("PROBE path={path} error={error}"),
    }
}

fn extract_ids(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    collect(&value, &mut ids);
    ids.sort();
    ids.dedup();
    ids
}

fn collect(value: &serde_json::Value, ids: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(id)) = map.get("id") {
                ids.push(id.clone());
            }
            for item in map.values() {
                collect(item, ids);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect(item, ids);
            }
        }
        _ => {}
    }
}

#[tokio::test]
#[ignore = "联网探测：用号池里的账号打上游模型列表"]
async fn upstream_models_probe() {
    if !prepare_env() {
        println!("SKIP no DATABASE_URL");
        return;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("config");
    let pool = build_pool(&cfg).await.expect("db");
    let token = pick_token(&pool).await;
    println!("SKIP_CHECK token_present={}", token.is_some());
    println!("BASE_ENV={}", std::env::var(BASE_URL_ENV).unwrap_or_default());

    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: None,
        timeout_secs: Some(30),
    })
    .expect("client");

    probe(&client, token.as_deref(), MODELS).await;
    probe(&client, None, MODELS_ANON).await;
    pool.close().await;
}

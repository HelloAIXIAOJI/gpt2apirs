//! 端到端演示：构造配置  生成 token  求解 PoW  独立校验。
//!
//! 运行：
//!
//! ```bash
//! cargo run -p gpt2apirs-protocol --bin pow-demo
//! ```

use std::time::Instant;

use sha3::{Digest, Sha3_512};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

fn main() -> anyhow::Result<()> {
    println!("=== 1. parse_pow_resources ===");
    let html = r#"<html data-build="c/abc123def/_">
        <script src="/cdn/assets/main.js"></script>
        <script src="/backend-api/sentinel/sdk.js"></script></html>"#;
    let (scripts, data_build) = gpt2apirs_protocol::parse_pow_resources(html);
    println!("  script_sources = {scripts:?}");
    println!("  data_build     = {data_build:?}");

    println!("=== 2. build_pow_config ===");
    let config = gpt2apirs_protocol::build_pow_config(USER_AGENT, Some(&scripts), &data_build);
    println!("  元素个数 = {}", config.len());
    let compact = gpt2apirs_protocol::python_json_compact(&serde_json::Value::Array(config.clone()));
    println!("  JSON 长度 = {}", compact.len());
    println!("  前 160 字符 = {}", &compact[..compact.len().min(160)]);

    println!("=== 3. build_legacy_requirements_token ===");
    let legacy = gpt2apirs_protocol::build_legacy_requirements_token(&config);
    println!("  token 前缀 = {}", &legacy[..14]);
    println!("  token 总长 = {}", legacy.len());

    println!("=== 4. PoW 求解 + 独立校验 ===");
    let seed = "demo-seed-0123456789abcdef";
    for difficulty in ["0f", "00ff", "000fff"] {
        let started = Instant::now();
        let answer = gpt2apirs_protocol::pow_generate(seed, difficulty, &config, 5_000_000)?;
        let elapsed = started.elapsed();
        let nonce = answer.nonce.map(|n| n.to_string()).unwrap_or_else(|| String::from("-"));
        println!(
            "  difficulty={difficulty:8} solved={:5} nonce={nonce:>6} 耗时={elapsed:?}",
            answer.solved
        );

        let target = decode_hex(difficulty)?;
        let mut hasher = Sha3_512::new();
        hasher.update(seed.as_bytes());
        hasher.update(answer.answer.as_bytes());
        let digest = hasher.finalize();
        let n = target.len();
        let meets = digest[..n] <= target[..];
        println!("    digest[:{n}] = {}  target = {}  满足={meets}", hex(&digest[..n]), hex(&target));
        assert!(meets, "难度校验失败");
    }

    println!("=== 5. build_proof_token ===");
    let proof = gpt2apirs_protocol::build_proof_token(seed, "000fff", &config)?;
    println!("  token 前缀 = {}", &proof[..8]);
    println!("  token 总长 = {}", proof.len());

    println!("全部通过。");
    Ok(())
}

fn decode_hex(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(2) {
        out.push(u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?);
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

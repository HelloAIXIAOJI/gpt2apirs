//! PoW 求解性能基准。
//!
//! **必须用 release 跑**，否则数字会差 50 倍以上（debug 未优化）：
//!
//! ```bash
//! cargo test --release -p gpt2apirs-protocol --test bench_rust -- --nocapture
//! ```
//!
//! 结果写入 `bench_rust.txt`。

use std::time::Instant;

use serde_json::Value;

#[test]
fn bench_rust_pow() {
    if cfg!(debug_assertions) {
        eprintln!("跳过基准：debug 构建的数字没有意义，请用 --release 运行");
        return;
    }

    let raw = include_str!("pow_bench_config.json");
    let parsed: Value = serde_json::from_str(raw).unwrap();
    let config = parsed["config"].as_array().unwrap().clone();

    // (标签, seed, difficulty, 运行次数)
    // 第一行是真实抓包难度；后两行是合成的高难度，用于看吞吐。
    let cases = [
        ("real", "0.05139335793793165", "06e75e", 20u32),
        ("mid", "bench-seed", "000fff", 3),
        ("high", "bench-seed", "0000ffff", 1),
    ];

    let mut out = String::new();
    for (label, seed, difficulty, runs) in cases {
        let _ = gpt2apirs_protocol::pow_generate(seed, difficulty, &config, 50_000_000);
        let mut samples = Vec::new();
        let mut nonce = 0u64;
        for _ in 0..runs {
            let start = Instant::now();
            let ans =
                gpt2apirs_protocol::pow_generate(seed, difficulty, &config, 50_000_000).unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            nonce = ans.nonce.unwrap_or(u64::MAX);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = samples[samples.len() / 2];
        let iters = nonce + 1;
        out.push_str(&format!(
            "{label}\tdifficulty={difficulty}\titers={iters}\tmedian_ms={median:.3}\tus_per_iter={:.3}\n",
            median * 1000.0 / iters as f64
        ));
    }
    std::fs::write("bench_rust.txt", &out).unwrap();
    print!("{out}");
}

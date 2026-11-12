//! PoW 求解的逐字节回归向量。
//!
//! 向量是一组固定的「输入  期望 proof token」，覆盖不同难度与配置。
//! 只要有一个字节变了，解析上游时就会全线失败，所以这里逐字节比对。

use base64::Engine as _;

#[derive(Debug, serde::Deserialize)]
struct Vector {
    name: String,
    seed: String,
    difficulty: String,
    config: Vec<serde_json::Value>,
    limit: u64,
    #[serde(rename = "configJson")]
    config_json: String,
    answer: String,
    solved: bool,
    #[serde(rename = "proofToken", default)]
    proof_token: Option<String>,
    #[serde(rename = "legacyToken", default)]
    legacy_token: Option<String>,
    #[serde(rename = "legacyConfig", default)]
    legacy_config: Option<Vec<serde_json::Value>>,
}

fn load_vectors() -> Vec<Vector> {
    let raw = include_str!("vectors.json");
    serde_json::from_str(raw).expect("vectors.json 解析失败")
}

/// 浮点序列化必须与 Python 一致。
#[test]
fn python_float_formatting() {
    // (值, Python repr)
    let cases: &[(f64, &str)] = &[
        (123456.789, "123456.789"),
        (1762500000000.0, "1762500000000.0"),
        (0.6394267984578837, "0.6394267984578837"),
        (0.0, "0.0"),
        (1.0, "1.0"),
        (-1.5, "-1.5"),
        (1.23456789e-7, "1.23456789e-07"),
        (1e17, "1e+17"),
        (1e-5, "1e-05"),
        (1e16, "1e+16"),
        (0.0001, "0.0001"),
        (1e-4, "0.0001"),
    ];
    for (value, expected) in cases {
        let actual = gpt2apirs_protocol::python_float_repr(*value);
        assert_eq!(
            &actual, expected,
            "float {value} 格式化不一致：Rust={actual} Python={expected}"
        );
    }
}

/// 整体 JSON 序列化必须与 Python `json.dumps(separators=(",",":"))` 一致。
#[test]
fn config_json_matches_python() {
    for vector in load_vectors() {
        let actual = gpt2apirs_protocol::python_json_compact(
            &serde_json::Value::Array(vector.config.clone()),
        );
        assert_eq!(
            actual, vector.config_json,
            "向量 {} 的 configJson 不一致",
            vector.name
        );
    }
}

/// 核心：求解结果必须与 Python 逐字节一致。
#[test]
fn pow_answer_matches_python() {
    let vectors = load_vectors();
    assert!(!vectors.is_empty(), "测试向量为空");

    for vector in vectors {
        let result = gpt2apirs_protocol::pow_generate(
            &vector.seed,
            &vector.difficulty,
            &vector.config,
            vector.limit,
        )
        .unwrap_or_else(|err| panic!("向量 {} 求解出错：{err}", vector.name));

        assert_eq!(
            result.solved, vector.solved,
            "向量 {} 的 solved 不一致（Rust={} Python={}）",
            vector.name, result.solved, vector.solved
        );
        assert_eq!(
            result.answer, vector.answer,
            "向量 {} 的 answer 不一致\nRust  = {}\nPython= {}",
            vector.name, result.answer, vector.answer
        );
    }
}

/// 验证 answer 确实是 base64 编码的合法 JSON，且第 3/9 项被替换成 nonce/nonce>>1。
#[test]
fn answer_is_valid_json_with_nonce() {
    let base64_engine = base64::engine::general_purpose::STANDARD;

    for vector in load_vectors() {
        if !vector.solved {
            continue;
        }
        let result = gpt2apirs_protocol::pow_generate(
            &vector.seed,
            &vector.difficulty,
            &vector.config,
            vector.limit,
        )
        .expect("求解失败");

        let decoded = base64_engine.decode(&result.answer).expect("answer 不是合法 base64");
        let parsed: serde_json::Value =
            serde_json::from_slice(&decoded).expect("answer 解码后不是合法 JSON");
        let array = parsed.as_array().expect("answer 不是 JSON 数组");

        let nonce = result.nonce.expect("solved 必须有 nonce");
        assert_eq!(
            array[3].as_u64(),
            Some(nonce),
            "向量 {} 第 3 项应为 nonce",
            vector.name
        );
        assert_eq!(
            array[9].as_u64(),
            Some(nonce >> 1),
            "向量 {} 第 9 项应为 nonce>>1",
            vector.name
        );
        assert_eq!(array.len(), vector.config.len(), "配置长度应保持不变");
    }
}

/// 验证难度目标被真正满足。
#[test]
fn answer_meets_difficulty() {
    use sha3::{Digest, Sha3_512};
    let base64_engine = base64::engine::general_purpose::STANDARD;

    for vector in load_vectors() {
        if !vector.solved {
            continue;
        }
        let result = gpt2apirs_protocol::pow_generate(
            &vector.seed,
            &vector.difficulty,
            &vector.config,
            vector.limit,
        )
        .expect("求解失败");

        let target = hex::decode(&vector.difficulty).expect("difficulty 非法");
        let mut hasher = Sha3_512::new();
        hasher.update(vector.seed.as_bytes());
        hasher.update(result.answer.as_bytes());
        let digest = hasher.finalize();
        let n = target.len();
        assert!(
            digest[..n] <= target[..],
            "向量 {} 未满足难度：digest[:{n}]={:02x?} > target={:02x?}",
            vector.name,
            &digest[..n],
            &target[..]
        );
        let _ = base64_engine;
    }
}

// 真实场景向量：由真实 build_pow_config() 现场生成后固化。
// 配置里包含 random.random() / perf_counter() 产生的浮点，是浮点序列化的真实考验。

fn load_live_vectors() -> Vec<Vector> {
    let raw = include_str!("vectors_live.json");
    serde_json::from_str(raw).expect("vectors_live.json 解析失败")
}

/// 真实随机配置的 JSON 序列化必须与 Python 逐字节一致。
#[test]
fn live_config_json_matches_python() {
    let vectors = load_live_vectors();
    assert_eq!(vectors.len(), 12, "真实向量数量应为 12");

    let mut mismatches = Vec::new();
    for vector in &vectors {
        let actual =
            gpt2apirs_protocol::python_json_compact(&serde_json::Value::Array(vector.config.clone()));
        if actual != vector.config_json {
            mismatches.push(vector.name.clone());
        }
    }
    assert!(
        mismatches.is_empty(),
        "以下真实向量的 configJson 与 Python 不一致：{mismatches:?}"
    );
}

/// 真实随机配置的 PoW 求解结果必须与 Python 逐字节一致。
#[test]
fn live_pow_answer_matches_python() {
    let vectors = load_live_vectors();

    for vector in &vectors {
        let result = gpt2apirs_protocol::pow_generate(
            &vector.seed,
            &vector.difficulty,
            &vector.config,
            vector.limit,
        )
        .unwrap_or_else(|err| panic!("真实向量 {} 求解出错：{err}", vector.name));

        assert_eq!(
            result.solved, vector.solved,
            "真实向量 {} 的 solved 不一致",
            vector.name
        );
        assert_eq!(
            result.answer, vector.answer,
            "真实向量 {} 的 answer 不一致\nRust  = {}\nPython= {}",
            vector.name, result.answer, vector.answer
        );
    }
}

/// 端到端：完整 proof token（含 gAAAAAB 前缀）必须与 Python 一致。
#[test]
fn live_proof_token_matches_python() {
    let vectors = load_live_vectors();
    let mut checked = 0;

    for vector in &vectors {
        let Some(expected) = vector.proof_token.as_deref() else {
            continue;
        };
        if !vector.solved {
            continue;
        }
        let actual = gpt2apirs_protocol::build_proof_token(
            &vector.seed,
            &vector.difficulty,
            &vector.config,
        )
        .unwrap_or_else(|err| panic!("向量 {} build_proof_token 失败：{err}", vector.name));

        assert_eq!(
            actual, expected,
            "向量 {} 的 proof token 不一致",
            vector.name
        );
        checked += 1;
    }
    assert!(checked >= 10, "应至少验证 10 条 proof token，实际 {checked}");
}

/// 端到端：legacy requirements token 的 base64 载荷必须能还原成 Python 的配置 JSON。
#[test]
fn live_legacy_token_matches_python() {
    let vectors = load_live_vectors();
    let engine = base64::engine::general_purpose::STANDARD;
    let mut checked = 0;

    for vector in &vectors {
        let (Some(expected), Some(config)) =
            (vector.legacy_token.as_deref(), vector.legacy_config.as_ref())
        else {
            continue;
        };

        let actual = gpt2apirs_protocol::build_legacy_requirements_token(config);
        assert_eq!(
            actual, expected,
            "向量 {} 的 legacy token 不一致",
            vector.name
        );

        // 再验证前缀能被剥掉并解出与 legacyConfig 一致的 JSON。
        // 注意：legacyConfig 是 Python 在生成 legacy token 时内部独立产生的随机配置，
        // 与 vector.config 不是同一份，因此只能与 legacyConfig 自身比对。
        let payload = &actual[gpt2apirs_protocol::LEGACY_REQUIREMENTS_PREFIX.len()..];
        let decoded = engine.decode(payload).expect("legacy token 载荷不是合法 base64");
        let decoded_json = String::from_utf8(decoded).expect("载荷不是 UTF-8");
        let expected_json =
            gpt2apirs_protocol::python_json_compact(&serde_json::Value::Array(config.clone()));
        assert_eq!(
            decoded_json, expected_json,
            "向量 {} legacy 载荷与 legacyConfig 序列化不一致",
            vector.name
        );
        checked += 1;
    }
    assert!(checked >= 10, "应至少验证 10 条 legacy token，实际 {checked}");
}
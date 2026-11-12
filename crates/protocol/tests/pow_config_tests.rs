//! `parse_pow_resources` / `build_pow_config` 与 Python 的对比测试。

use chrono::DateTime;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ParseCase {
    name: String,
    html: String,
    #[serde(rename = "scriptSources")]
    script_sources: Vec<String>,
    #[serde(rename = "dataBuild")]
    data_build: String,
}

#[derive(Debug, Deserialize)]
struct TimeCase {
    #[serde(rename = "epochSeconds")]
    epoch_seconds: f64,
    expected: String,
}

#[derive(Debug, Deserialize)]
struct ConfigExpected {
    #[serde(rename = "timeCases")]
    time_cases: Vec<TimeCase>,
    #[serde(rename = "configSample")]
    config_sample: Vec<serde_json::Value>,
    #[serde(rename = "configSampleJson")]
    config_sample_json: String,
    #[serde(rename = "crossCheckConfig")]
    cross_check_config: Vec<serde_json::Value>,
    #[serde(rename = "crossCheckLegacyToken")]
    cross_check_legacy_token: String,
}

/// `parse_pow_resources` 必须与 Python 完全一致（含默认脚本与 data-build 回退）。
#[test]
fn parse_pow_resources_matches_python() {
    let raw = include_str!("parse_expected.json");
    let cases: Vec<ParseCase> = serde_json::from_str(raw).expect("parse_expected.json 解析失败");
    assert!(!cases.is_empty());

    for case in cases {
        let (scripts, data_build) = gpt2apirs_protocol::parse_pow_resources(&case.html);
        assert_eq!(
            scripts, case.script_sources,
            "{} 的 script_sources 不一致",
            case.name
        );
        assert_eq!(
            data_build, case.data_build,
            "{} 的 data_build 不一致",
            case.name
        );
    }
}

/// `legacy_parse_time` 必须与 Python 的 strftime 完全一致（含 -5 固定偏移）。
#[test]
fn legacy_parse_time_matches_python() {
    let raw = include_str!("config_expected.json");
    let expected: ConfigExpected = serde_json::from_str(raw).expect("config_expected.json 解析失败");

    for case in &expected.time_cases {
        let secs = case.epoch_seconds.floor() as i64;
        let nanos = ((case.epoch_seconds - case.epoch_seconds.floor()) * 1e9).round() as u32;
        let dt = DateTime::from_timestamp(secs, nanos)
            .unwrap_or_else(|| panic!("时间戳 {} 非法", case.epoch_seconds))
            .fixed_offset();
        let actual = gpt2apirs_protocol::legacy_parse_time(dt);
        assert_eq!(
            actual, case.expected,
            "时间戳 {} 格式化不一致",
            case.epoch_seconds
        );
    }
}

/// `build_pow_config` 的结构必须与 Python 一致：25 个元素，类型逐位匹配。
#[test]
fn build_pow_config_structure() {
    let config = gpt2apirs_protocol::build_pow_config(
        "Mozilla/5.0 test",
        Some(&["https://chatgpt.com/backend-api/sentinel/sdk.js".to_string()]),
        "c/abcdef12/_",
    );

    assert_eq!(config.len(), 25, "配置必须是 25 个元素");

    // 与 Python 记录的类型序列逐位比对：
    // ['int','str','int','int','str','str','str','str','str','float',
    //  'str','str','str','float','str','str','int','float',
    //  'int','int','int','int','int','int','int']
    let expected: [&str; 25] = [
        "int", "str", "int", "int", "str", "str", "str", "str", "str", "float",
        "str", "str", "str", "float", "str", "str", "int", "float",
        "int", "int", "int", "int", "int", "int", "int",
    ];
    for (index, want) in expected.iter().enumerate() {
        let actual = match &config[index] {
            serde_json::Value::Number(n) if n.is_f64() => "float",
            serde_json::Value::Number(_) => "int",
            serde_json::Value::String(_) => "str",
            serde_json::Value::Bool(_) => "bool",
            _ => "other",
        };
        assert_eq!(actual, *want, "第 {index} 个元素类型应为 {want}，实际 {actual}");
    }

    // 固定字段
    assert_eq!(config[2], serde_json::json!(4294705152_i64), "第 2 项固定值");
    assert_eq!(config[3], serde_json::json!(1), "第 3 项初始为 1");
    assert_eq!(config[7], serde_json::json!("en-US"));
    assert_eq!(config[8], serde_json::json!("en-US,es-US,en,es"));
    assert_eq!(config[15], serde_json::json!(""));
    assert_eq!(config[24], serde_json::json!(0), "末位 0 表示 edge/chrome");
    for index in 18..=24 {
        assert_eq!(config[index], serde_json::json!(0), "第 {index} 项应为 0");
    }

    // 横向校验：屏幕分辨率之和必须在候选集合里
    let screen_sum = config[0].as_i64().unwrap();
    let valid: Vec<i64> = gpt2apirs_protocol::SCREEN_RESOLUTIONS
        .iter()
        .map(|r| r[0] + r[1])
        .collect();
    assert!(valid.contains(&screen_sum), "屏幕和 {screen_sum} 不在候选集合 {valid:?}");

    // 核心数必须在候选集合里
    let core = config[16].as_i64().unwrap();
    assert!(gpt2apirs_protocol::CORES.contains(&core), "核心数 {core} 不在候选集合");

    // navigator / window / document 键必须在候选集合里
    let nav = config[10].as_str().unwrap();
    assert!(gpt2apirs_protocol::NAVIGATOR_KEYS.contains(&nav));
    let win = config[12].as_str().unwrap();
    assert!(gpt2apirs_protocol::WINDOW_KEYS.contains(&win));
    let doc = config[11].as_str().unwrap();
    assert!(gpt2apirs_protocol::DOCUMENT_KEYS.contains(&doc));

    // uuid 必须是合法 v4 字符串
    let uuid = config[14].as_str().unwrap();
    assert_eq!(uuid.len(), 36, "uuid 长度应为 36");
}

/// Python 生成的配置交给 Rust 生成 legacy token，必须与 Python 完全一致。
#[test]
fn cross_check_legacy_token() {
    let raw = include_str!("config_expected.json");
    let expected: ConfigExpected = serde_json::from_str(raw).expect("config_expected.json 解析失败");

    let actual =
        gpt2apirs_protocol::build_legacy_requirements_token(&expected.cross_check_config);
    assert_eq!(
        actual, expected.cross_check_legacy_token,
        "Python 配置经 Rust 生成的 legacy token 与 Python 不一致"
    );
}

/// Python 记录的配置样本，经 Rust 序列化后必须与其 JSON 表示一致。
#[test]
fn config_sample_json_matches_python() {
    let raw = include_str!("config_expected.json");
    let expected: ConfigExpected = serde_json::from_str(raw).expect("config_expected.json 解析失败");

    let actual = gpt2apirs_protocol::python_json_compact(&serde_json::Value::Array(
        expected.config_sample.clone(),
    ));
    assert_eq!(actual, expected.config_sample_json);
}

/// Rust 生成的配置能走完整链路：config -> legacy token -> 解出同样的 JSON。
#[test]
fn build_pow_config_roundtrip() {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;

    for _ in 0..20 {
        let config = gpt2apirs_protocol::build_pow_config(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/143.0.0.0",
            Some(&[gpt2apirs_protocol::DEFAULT_POW_SCRIPT.to_string()]),
            "c/deadbeef/_"
        );

        let token = gpt2apirs_protocol::build_legacy_requirements_token(&config);
        assert!(token.starts_with(gpt2apirs_protocol::LEGACY_REQUIREMENTS_PREFIX));

        let payload = &token[gpt2apirs_protocol::LEGACY_REQUIREMENTS_PREFIX.len()..];
        let decoded = engine.decode(payload).expect("载荷应可解码");
        let decoded_json = String::from_utf8(decoded).expect("载荷应为 UTF-8");
        let expected_json = gpt2apirs_protocol::python_json_compact(
            &serde_json::Value::Array(config.clone()),
        );
        assert_eq!(decoded_json, expected_json, "往返后 JSON 应一致");

        // 解码后必须能被 serde_json 解析回来
        let _: serde_json::Value =
            serde_json::from_str(&decoded_json).expect("解码结果应为合法 JSON");
    }
}

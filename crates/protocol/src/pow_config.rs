//! Sentinel PoW 的配置构造与资源解析。
//!
//!
//! # 随机性为什么可注入
//!
//! 全局 RNG 无法在测试里复现。因此本模块：
//!
//! - [RNG 与时钟均可注入][PowConfigContext]，便于测试与复现；
//! - 提供 [`build_pow_config`] 便利函数，用系统随机源与真实时钟；
//!
//! 配置中的随机字段本身不被服务端校验（PoW 只校验哈希），
//! 因此不要求 RNG 序列与 Python 一致。

use chrono::{DateTime, FixedOffset};
use rand::Rng;
use regex::Regex;
use serde_json::{Value, json};
use uuid::Uuid;

/// 默认 PoW 脚本地址（上游实测值）。
pub const DEFAULT_POW_SCRIPT: &str = "https://chatgpt.com/backend-api/sentinel/sdk.js";

/// 上报的 `hardwareConcurrency` 候选值。
pub const CORES: [i64; 4] = [8, 16, 24, 32];

/// 上报的 document 键候选值。
pub const DOCUMENT_KEYS: [&str; 3] = [
    "__reactContainer$fzelfjyxej8",
    "_reactListening5dehydibo78",
    "location",
];

/// 上报的屏幕分辨率候选值。
pub const SCREEN_RESOLUTIONS: [[i64; 2]; 4] = [
    [1920, 1080],
    [1440, 900],
    [2560, 1440],
    [3840, 2160],
];
/// `navigator` 指纹候选值。
pub const NAVIGATOR_KEYS: [&str; 34] = [
    "registerProtocolHandler−function registerProtocolHandler() { [native code] }",
    "storage−[object StorageManager]",
    "locks−[object LockManager]",
    "appCodeName−Mozilla",
    "permissions−[object Permissions]",
    "share−function share() { [native code] }",
    "webdriver−false",
    "managed−[object NavigatorManagedData]",
    "canShare−function canShare() { [native code] }",
    "vendor−Google Inc.",
    "mediaDevices−[object MediaDevices]",
    "vibrate−function vibrate() { [native code] }",
    "storageBuckets−[object StorageBucketManager]",
    "mediaCapabilities−[object MediaCapabilities]",
    "cookieEnabled−true",
    "virtualKeyboard−[object VirtualKeyboard]",
    "product−Gecko",
    "presentation−[object Presentation]",
    "onLine−true",
    "mimeTypes−[object MimeTypeArray]",
    "credentials−[object CredentialsContainer]",
    "serviceWorker−[object ServiceWorkerContainer]",
    "keyboard−[object Keyboard]",
    "gpu−[object GPU]",
    "doNotTrack",
    "serial−[object Serial]",
    "pdfViewerEnabled−true",
    "language−zh-CN",
    "geolocation−[object Geolocation]",
    "userAgentData−[object NavigatorUAData]",
    "getUserMedia−function getUserMedia() { [native code] }",
    "sendBeacon−function sendBeacon() { [native code] }",
    "hardwareConcurrency−32",
    "windowControlsOverlay−[object WindowControlsOverlay]",
];

/// `window` 键候选值。
pub const WINDOW_KEYS: [&str; 43] = [
    "0",
    "window",
    "self",
    "document",
    "name",
    "location",
    "customElements",
    "history",
    "navigation",
    "innerWidth",
    "innerHeight",
    "scrollX",
    "scrollY",
    "visualViewport",
    "screenX",
    "screenY",
    "outerWidth",
    "outerHeight",
    "devicePixelRatio",
    "screen",
    "chrome",
    "navigator",
    "onresize",
    "performance",
    "crypto",
    "indexedDB",
    "sessionStorage",
    "localStorage",
    "scheduler",
    "alert",
    "atob",
    "btoa",
    "fetch",
    "matchMedia",
    "postMessage",
    "queueMicrotask",
    "requestAnimationFrame",
    "setInterval",
    "setTimeout",
    "caches",
    "__NEXT_DATA__",
    "__BUILD_MANIFEST",
    "__NEXT_PRELOADREADY",
];
/// 构造配置时可注入的上下文，便于测试与复现。
#[derive(Debug, Clone)]
pub struct PowConfigContext {
    /// 当前的“伪本地时间”（UTC-5 固定偏移）。Python 用 `timezone(timedelta(hours=-5))`，
    /// 即固定偏移而非真实 EST/EDT，因此这里也用固定偏移。
    pub now_utc: DateTime<FixedOffset>,
    /// 对应 Python `time.perf_counter() * 1000`。
    pub perf_counter_ms: f64,
    /// 对应 Python `time.time() * 1000`。
    pub epoch_ms: f64,
    /// 对应 Python `new_uuid()`（uuid4 字符串）。
    pub uuid: String,
}

impl PowConfigContext {
    /// 用系统时钟与随机 uuid 构造上下文。
    pub fn now() -> Self {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        Self {
            now_utc: chrono::Utc::now().fixed_offset(),
            perf_counter_ms: monotonic_ms(),
            epoch_ms,
            uuid: Uuid::new_v4().to_string(),
        }
    }
}

/// 单调时钟毫秒值，近似 Python 的 `time.perf_counter() * 1000`。
fn monotonic_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64() * 1000.0
}

/// 对应 Python `_legacy_parse_time()`：
///
/// ```python
/// now = datetime.now(timezone(timedelta(hours=-5)))
/// return now.strftime("%a %b %d %Y %H:%M:%S") + " GMT-0500 (Eastern Standard Time)"
/// ```
pub fn legacy_parse_time(now: DateTime<FixedOffset>) -> String {
    let west5 = FixedOffset::west_opt(5 * 3600).expect("固定偏移 -5h 合法");
    let local = now.with_timezone(&west5);
    format!(
        "{} GMT-0500 (Eastern Standard Time)",
        local.format("%a %b %d %Y %H:%M:%S")
    )
}

/// 对应 Python `parse_pow_resources(html) -> (script_sources, data_build)`。
///
/// - 提取所有 `<script src="...">` 的 src，保持出现顺序；
/// - 从 src 中匹配 `c/[^/]*/_` 作为 data_build（多次匹配以最后一次为准）；
/// - 若无 script src，返回 [`DEFAULT_POW_SCRIPT`]；
/// - 若仍无 data_build，回退到 `<html ... data-build="...">`。
pub fn parse_pow_resources(html: &str) -> (Vec<String>, String) {
    let stripped = strip_html_comments(html);
    let tag_re = Regex::new(r"(?is)<script\b[^>]*>").expect("script 正则");
    let src_re = Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
        .expect("src 正则");
    let build_re = Regex::new(r"c/[^/]*/_").expect("data-build 正则");

    let mut script_sources = Vec::new();
    let mut data_build = String::new();

    for tag in tag_re.find_iter(&stripped) {
        let Some(caps) = src_re.captures(tag.as_str()) else {
            continue;
        };
        let src = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if src.is_empty() {
            continue;
        }
        script_sources.push(src.clone());
        if let Some(found) = build_re.find(&src) {
            data_build = found.as_str().to_string();
        }
    }

    if script_sources.is_empty() {
        script_sources.push(DEFAULT_POW_SCRIPT.to_string());
    }

    if data_build.is_empty() {
        let attr_re =
            Regex::new(r#"(?is)<html[^>]*data-build\s*=\s*"([^"]*)""#).expect("html 属性正则");
        if let Some(caps) = attr_re.captures(&stripped) {
            data_build = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        }
    }

    (script_sources, data_build)
}

/// 去掉 HTML 注释，避免误匹配被注释掉的 script 标签。
fn strip_html_comments(html: &str) -> String {
    let re = Regex::new(r"(?s)<!--.*?-->").expect("注释正则");
    re.replace_all(html, "").into_owned()
}
/// 构造 25 元素 PoW 配置。
///
/// 随机源与时钟由调用方注入：
/// - `rng`：任意 `rand::Rng`，生产用线程随机源，测试用固定种子；
/// - `ctx`：时间、性能计数器、uuid。
pub fn build_pow_config_with<R: Rng + ?Sized>(
    rng: &mut R,
    user_agent: &str,
    script_sources: Option<&[String]>,
    data_build: &str,
    ctx: &PowConfigContext,
) -> Vec<Value> {
    let navigator_key = NAVIGATOR_KEYS[rng.gen_range(0..NAVIGATOR_KEYS.len())];
    let window_key = WINDOW_KEYS[rng.gen_range(0..WINDOW_KEYS.len())];
    let document_key = DOCUMENT_KEYS[rng.gen_range(0..DOCUMENT_KEYS.len())];

    let script_source = match script_sources {
        Some(sources) if !sources.is_empty() => {
            sources[rng.gen_range(0..sources.len())].clone()
        }
        _ => DEFAULT_POW_SCRIPT.to_string(),
    };

    let resolution = SCREEN_RESOLUTIONS[rng.gen_range(0..SCREEN_RESOLUTIONS.len())];
    let screen_sum = resolution[0] + resolution[1];
    // Rust 2024 中 `gen` 是保留字，用 gen_range 等价替代 random.random()
    let random_value = rng.gen_range(0.0_f64..1.0);
    let core = CORES[rng.gen_range(0..CORES.len())];

    vec![
        json!(screen_sum),                                  // 0
        json!(legacy_parse_time(ctx.now_utc)),              // 1
        json!(4294705152_i64),                              // 2
        json!(1),                                           // 3 -> PoW 替换为 nonce
        json!(user_agent),                                  // 4
        json!(script_source),                               // 5
        json!(data_build),                                  // 6
        json!("en-US"),                                     // 7
        json!("en-US,es-US,en,es"),                         // 8
        json!(random_value),                                // 9 -> PoW 替换为 nonce>>1
        json!(navigator_key),                               // 10
        json!(document_key),                                // 11
        json!(window_key),                                  // 12
        json!(ctx.perf_counter_ms),                         // 13
        json!(ctx.uuid),                                    // 14
        json!(""),                                          // 15
        json!(core),                                        // 16
        json!(ctx.epoch_ms - ctx.perf_counter_ms),          // 17
        json!(0),                                           // 18
        json!(0),                                           // 19
        json!(0),                                           // 20
        json!(0),                                           // 21
        json!(0),                                           // 22
        json!(0),                                           // 23
        json!(0),                                           // 24: 0=edge/chrome, 1=firefox
    ]
}

/// 便捷版：使用线程随机源与系统时钟。
pub fn build_pow_config(
    user_agent: &str,
    script_sources: Option<&[String]>,
    data_build: &str,
) -> Vec<Value> {
    let mut rng = rand::thread_rng();
    let ctx = PowConfigContext::now();
    build_pow_config_with(&mut rng, user_agent, script_sources, data_build, &ctx)
}
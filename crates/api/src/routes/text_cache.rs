//! 文本结果缓存。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpt2apirs_generate::text::TextResult;

/// 单条缓存。
#[derive(Clone)]
struct Entry {
    text: String,
    reasoning: String,
    conversation_id: String,
    deltas: Vec<gpt2apirs_protocol::text_stream::Delta>,
    at: Instant,
}

/// 容量上限。
const MAX_ENTRIES: usize = 256;

fn store() -> &'static Mutex<HashMap<u64, Entry>> {
    static STORE: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 计算缓存键。
/// 缓存键。
///
/// 必须带 `effort`：同一句话先请求 `low`、再请求 `high`，
/// 不带就会命中第一次的缓存，把「思考强度」这个参数吞掉。
/// （同类事故：verikun #427 —— 它的缓存键缺协议分区，chat 请求拿到了 responses 的结果。）
///
/// **不加**协议分区：我们缓存的是内部 `TextResult`，不是包装好的响应体，
/// 三个端点共用同一份缓存是对的；加了反而让同一句话在三个端点各存一份。
pub fn cache_key(model: &str, effort: Option<&str>, messages: &[(String, String)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut hasher);
    // None 与 "" 要区分开：一个是不传，一个是传了空串（归一化后其实一样，但别混）
    effort.unwrap_or("\u{0}").hash(&mut hasher);
    for (role, text) in messages {
        role.hash(&mut hasher);
        text.hash(&mut hasher);
    }
    hasher.finish()
}

/// 查缓存。`ttl` 为 0 时直接跳过。
pub fn get(key: u64, ttl: Duration) -> Option<TextResult> {
    if ttl.is_zero() {
        return None;
    }
    let guard = store().lock().ok()?;
    let entry = guard.get(&key)?;
    if entry.at.elapsed() > ttl {
        return None;
    }
    Some(TextResult {
        text: entry.text.clone(),
        reasoning: entry.reasoning.clone(),
        conversation_id: entry.conversation_id.clone(),
        deltas: entry.deltas.clone(),
        sse_raw: String::new(),
    })
}

/// 写缓存。`ttl` 为 0 时不写。
pub fn put(key: u64, result: &TextResult, ttl: Duration) {
    if ttl.is_zero() || result.text.trim().is_empty() {
        return;
    }
    let Ok(mut guard) = store().lock() else {
        return;
    };
    if guard.len() >= MAX_ENTRIES && !guard.contains_key(&key) {
        // 淘汰最旧的一条（不追求 LRU，够用且简单）
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, entry)| entry.at)
            .map(|(key, _)| *key)
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(
        key,
        Entry {
            text: result.text.clone(),
            reasoning: result.reasoning.clone(),
            conversation_id: result.conversation_id.clone(),
            deltas: result.deltas.clone(),
            at: Instant::now(),
        },
    );
}

/// 清空（测试用）。
pub fn clear() {
    if let Ok(mut guard) = store().lock() {
        guard.clear();
    }
}

/// 当前条数（诊断用）。
pub fn len() -> usize {
    store().lock().map(|guard| guard.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 缓存是进程级的，测试并行执行时不能互相 clear。
    // 做法：每个用例用自己的模型名做键空间，断言只针对自己的键。

    fn sample(text: &str) -> TextResult {
        TextResult {
            text: text.to_string(),
            reasoning: String::new(),
            conversation_id: "conv-1".to_string(),
            deltas: vec![gpt2apirs_protocol::text_stream::Delta::Text(text.to_string())],
            sse_raw: String::new(),
        }
    }

    fn key_of(scope: &str) -> u64 {
        cache_key(scope, None, &[("user".to_string(), "你好".to_string())])
    }

    #[test]
    fn zero_ttl_disables_cache() {
        let key = key_of("ttl-zero");
        put(key, &sample("x"), Duration::ZERO);
        assert!(get(key, Duration::from_secs(60)).is_none(), "TTL=0 不应写入");
        assert!(get(key, Duration::ZERO).is_none(), "TTL=0 不应读取");
    }

    #[test]
    fn round_trip() {
        let key = key_of("round-trip");
        put(key, &sample("答案"), Duration::from_secs(60));
        let hit = get(key, Duration::from_secs(60)).expect("应命中");
        assert_eq!(hit.text, "答案");
        assert_eq!(hit.conversation_id, "conv-1");
    }

    #[test]
    fn different_inputs_do_not_collide() {
        let a = cache_key("m", None, &[("user".to_string(), "A".to_string())]);
        let b = cache_key("m", None, &[("user".to_string(), "B".to_string())]);
        let c = cache_key("m2", None, &[("user".to_string(), "A".to_string())]);
        assert_ne!(a, b);
        assert_ne!(a, c, "模型不同也不应命中同一条");
    }

    /// 同一个提示词，思考强度不同就不能共用缓存。
    ///
    /// 这条是照 verikun #427 的教训写的：缓存键少一个维度，参数就被吞掉。
    #[test]
    fn effort_separates_cache_entries() {
        let msgs = [("user".to_string(), "同一个问题".to_string())];
        let low = cache_key("text", Some("low"), &msgs);
        let high = cache_key("text", Some("high"), &msgs);
        let none = cache_key("text", None, &msgs);
        assert_ne!(low, high, "low 与 high 必须分开");
        assert_ne!(low, none, "不传 effort 也要和传了的分开");
        assert_eq!(
            low,
            cache_key("text", Some("low"), &msgs),
            "同样的输入必须稳定命中同一条"
        );
    }

    #[test]
    fn expired_entries_are_missed() {
        let key = key_of("expired");
        put(key, &sample("旧"), Duration::from_secs(60));
        // 用一个极短 TTL 读取 → 应视为过期
        assert!(get(key, Duration::from_nanos(1)).is_none());
    }

    #[test]
    fn empty_text_is_not_cached() {
        let key = key_of("empty-text");
        put(key, &sample("   "), Duration::from_secs(60));
        assert!(get(key, Duration::from_secs(60)).is_none(), "空白文本不应进缓存");
    }
}

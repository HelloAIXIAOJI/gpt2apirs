//! 文本结果缓存。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpt2apirs_generate::text::TextResult;

/// 单条缓存。
#[derive(Clone)]
struct Entry {
    text: String,
    conversation_id: String,
    deltas: Vec<String>,
    at: Instant,
}

/// 容量上限。
const MAX_ENTRIES: usize = 256;

fn store() -> &'static Mutex<HashMap<u64, Entry>> {
    static STORE: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 计算缓存键。
pub fn cache_key(model: &str, messages: &[(String, String)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut hasher);
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
            conversation_id: "conv-1".to_string(),
            deltas: vec![text.to_string()],
            sse_raw: String::new(),
        }
    }

    fn key_of(scope: &str) -> u64 {
        cache_key(scope, &[("user".to_string(), "你好".to_string())])
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
        let a = cache_key("m", &[("user".to_string(), "A".to_string())]);
        let b = cache_key("m", &[("user".to_string(), "B".to_string())]);
        let c = cache_key("m2", &[("user".to_string(), "A".to_string())]);
        assert_ne!(a, b);
        assert_ne!(a, c, "模型不同也不应命中同一条");
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

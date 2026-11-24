//! 官网「记忆」的解析（管理后台「记忆」页用）。
//!
//! 实测（2026-11）：`GET /backend-api/memories` 返回
//! `{memories: [...], memory_max_tokens, memory_num_tokens}`；
//! 删除是 `DELETE /backend-api/memories/{id}` → `{"success":true}`，
//! 且**不存在的 id 也回 success**（幂等，所以「删掉了」只能以列表里消失为准）。
//!
//! 记忆条目的字段名上游换过几次，这里按已知的几种取，取不到就留空 ——
//! 解析失败导致整页空白，比字段少一个严重得多。

use serde_json::Value;

/// 一条已保存的记忆。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// 记忆 ID（拿不到时为空串：能看不能删）。
    pub id: String,
    /// 记忆正文。
    pub content: String,
}

/// 记忆页数据。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryPage {
    /// 条目。
    pub entries: Vec<MemoryEntry>,
    /// 记忆容量（token）。
    pub max_tokens: i64,
    /// 已用（token）。
    pub num_tokens: i64,
}

impl MemoryPage {
    /// 已用百分比（0..=100）；容量未知时返回 None。
    pub fn used_percent(&self) -> Option<u32> {
        if self.max_tokens <= 0 {
            return None;
        }
        let percent = (self.num_tokens as f64 / self.max_tokens as f64 * 100.0).round();
        Some(percent.clamp(0.0, 100.0) as u32)
    }
}

/// 解析记忆列表响应。
pub fn parse(body: &Value) -> MemoryPage {
    let entries = body
        .get("memories")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_entry).collect())
        .unwrap_or_default();
    MemoryPage {
        entries,
        max_tokens: int_field(body, "memory_max_tokens"),
        num_tokens: int_field(body, "memory_num_tokens"),
    }
}

fn parse_entry(row: &Value) -> Option<MemoryEntry> {
    let content = ["content", "memory", "text", "value"]
        .iter()
        .find_map(|key| row.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let id = ["id", "memory_id"]
        .iter()
        .find_map(|key| row.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() && content.is_empty() {
        return None;
    }
    Some(MemoryEntry { id, content })
}

fn int_field(body: &Value, key: &str) -> i64 {
    body.get(key).and_then(Value::as_i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_empty_response() {
        // 真实响应（这台号还没有记忆）
        let body: Value = serde_json::from_str(
            r#"{"memories":[],"memory_max_tokens":5000000,"memory_num_tokens":0}"#,
        )
        .expect("JSON");
        let page = parse(&body);
        assert!(page.entries.is_empty());
        assert_eq!(page.max_tokens, 5_000_000);
        assert_eq!(page.used_percent(), Some(0));
    }

    #[test]
    fn accepts_alternate_field_names() {
        // 上游换过字段名：content / memory / text 都要认
        let body = serde_json::json!({
            "memories": [
                {"id": "m1", "content": "用户喜欢橘猫"},
                {"memory_id": "m2", "memory": "用户在北京"},
                {"id": "m3", "text": "用户用 Rust"}
            ],
            "memory_max_tokens": 100,
            "memory_num_tokens": 25
        });
        let page = parse(&body);
        assert_eq!(page.entries.len(), 3);
        assert_eq!(page.entries[0].id, "m1");
        assert_eq!(page.entries[1].id, "m2");
        assert_eq!(page.entries[2].content, "用户用 Rust");
        assert_eq!(page.used_percent(), Some(25));
    }

    #[test]
    fn entry_without_id_is_still_listed() {
        // 拿不到 id 的记忆也要显示（能看不能删），否则页面会莫名少几条
        let body = serde_json::json!({"memories": [{"content": "只有正文"}]});
        let page = parse(&body);
        assert_eq!(page.entries.len(), 1);
        assert!(page.entries[0].id.is_empty());
    }

    #[test]
    fn percent_is_clamped_and_none_without_capacity() {
        let page = MemoryPage { entries: vec![], max_tokens: 10, num_tokens: 999 };
        assert_eq!(page.used_percent(), Some(100));
        let unknown = MemoryPage { entries: vec![], max_tokens: 0, num_tokens: 5 };
        assert_eq!(unknown.used_percent(), None);
    }
}

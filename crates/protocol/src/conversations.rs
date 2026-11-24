//! 官网会话列表的解析（管理后台「会话」页用）。
//!
//! 上游返回：`{items: [...], total, limit, offset}`，item 里我们只关心
//! id / 标题 / 时间 / 几个布尔标记 —— mapping 之类的树形内容不往管理面带。
//!
//! 实测（2026-11）：`GET /backend-api/conversations?offset=0&limit=3&order=updated`。

use chrono::{DateTime, Utc};
use serde_json::Value;

/// 一条会话摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationSummary {
    /// 会话 ID。
    pub id: String,
    /// 标题（上游可能给空串或 null）。
    pub title: String,
    /// 创建时间。
    pub created_at: Option<DateTime<Utc>>,
    /// 最后更新时间。
    pub updated_at: Option<DateTime<Utc>>,
    /// 是否已归档。
    pub archived: bool,
    /// 是否星标。
    pub starred: bool,
    /// 是否临时会话（临时会话不落库、也删不掉，页面上要标出来）。
    pub temporary: bool,
    /// 是否「不记住」（is_do_not_remember）。
    pub no_memory: bool,
}

/// 一页会话。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationPage {
    /// 本页条目。
    pub items: Vec<ConversationSummary>,
    /// 上游统计的总数。
    pub total: i64,
    /// 上游回显的 limit。
    pub limit: i64,
    /// 上游回显的 offset。
    pub offset: i64,
}

impl ConversationPage {
    /// 标题为空时给个占位：表格里空标题看起来像渲染坏了。
    pub fn display_title(item: &ConversationSummary) -> String {
        if item.title.trim().is_empty() {
            "(无标题)".to_string()
        } else {
            item.title.clone()
        }
    }
}

/// 解析列表响应。
pub fn parse_page(body: &Value) -> ConversationPage {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_item).collect())
        .unwrap_or_default();
    ConversationPage {
        items,
        total: int_field(body, "total"),
        limit: int_field(body, "limit"),
        offset: int_field(body, "offset"),
    }
}

fn parse_item(row: &Value) -> Option<ConversationSummary> {
    let id = row.get("id").and_then(Value::as_str)?.trim().to_string();
    if id.is_empty() {
        // 没有 id 的条目删不了也没法定位，直接不要
        return None;
    }
    Some(ConversationSummary {
        id,
        title: row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        created_at: time_field(row, "create_time"),
        updated_at: time_field(row, "update_time"),
        archived: row.get("is_archived").and_then(Value::as_bool).unwrap_or(false),
        starred: row.get("is_starred").and_then(Value::as_bool).unwrap_or(false),
        temporary: row
            .get("is_temporary_chat")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        no_memory: row
            .get("is_do_not_remember")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn int_field(body: &Value, key: &str) -> i64 {
    body.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// 时间字段：列表接口给 ISO8601 字符串，详情接口给 epoch 秒 —— 两种都认。
fn time_field(row: &Value, key: &str) -> Option<DateTime<Utc>> {
    match row.get(key)? {
        Value::String(text) => DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|value| value.with_timezone(&Utc)),
        Value::Number(number) => {
            let secs = number.as_f64()?;
            let nanos = (secs.fract().max(0.0) * 1_000_000_000.0) as u32;
            DateTime::from_timestamp(secs.trunc() as i64, nanos)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 取自真实响应（删除了无关字段）
    const SAMPLE: &str = r#"{
        "items": [
            {"id": "6b057702-5f28-83ea-974f-55ca5e91fa74", "title": "橘猫窗台晴天",
             "create_time": "2026-11-24T11:37:38.902821Z", "update_time": "2026-11-24T11:37:52.633030Z",
             "is_archived": false, "is_starred": null, "is_temporary_chat": false, "is_do_not_remember": true},
            {"id": "abc", "title": null, "create_time": 1795520258.5, "update_time": null,
             "is_archived": true, "is_starred": true, "is_temporary_chat": true, "is_do_not_remember": false},
            {"title": "没有 id 的条目"}
        ],
        "total": 4, "limit": 3, "offset": 0
    }"#;

    #[test]
    fn parses_items_and_paging() {
        let body: Value = serde_json::from_str(SAMPLE).expect("JSON");
        let page = parse_page(&body);
        assert_eq!(page.total, 4);
        assert_eq!(page.limit, 3);
        assert_eq!(page.items.len(), 2, "没有 id 的条目要丢掉");
        let first = &page.items[0];
        assert_eq!(first.title, "橘猫窗台晴天");
        assert!(first.created_at.is_some(), "ISO 时间要能解析");
        assert!(first.no_memory);
        assert!(!first.archived);
        let second = &page.items[1];
        assert!(second.archived && second.starred && second.temporary);
        assert_eq!(ConversationPage::display_title(second), "(无标题)");
        assert!(second.created_at.is_some(), "epoch 秒也要能解析");
    }

    #[test]
    fn empty_or_broken_body_yields_empty_page() {
        assert!(parse_page(&Value::Null).items.is_empty());
        assert_eq!(parse_page(&serde_json::json!({"items": []})).total, 0);
    }
}

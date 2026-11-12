//! 联网搜索：请求体构造 + 结果解析。
//!
//! 搜索不是独立协议，而是「带 search 提示的普通会话」：prepare 与 conversation
//! 都要带 `system_hints: ["search"]`，conversation 再加 `force_use_search`。
//! 回答与来源都在会话详情里，所以要配合 GET conversation 轮询使用。

use serde_json::{Value, json};

use crate::util::{new_uuid, now_unix_secs_f64};

/// 搜索会话要带的提示（上游据此启用搜索）。
const SEARCH_HINTS: [&str; 1] = ["search"];

fn user_message(prompt: &str) -> Value {
    json!({
        "id": new_uuid(),
        "author": { "role": "user" },
        "create_time": now_unix_secs_f64(),
        "content": { "content_type": "text", "parts": [prompt] },
        "metadata": {
            "developer_mode_connector_ids": [],
            "selected_github_repos": [],
            "selected_all_github_repos": false,
            "system_hints": SEARCH_HINTS,
            "serialization_metadata": { "custom_symbol_offsets": [] }
        }
    })
}

/// `conversation/prepare` 的请求体（换 conduit_token 用）。
pub fn build_search_prepare_payload(prompt: &str, model: &str) -> Value {
    json!({
        "action": "next",
        "fork_from_shared_post": false,
        "parent_message_id": "client-created-root",
        "model": model,
        "client_prepare_state": "success",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "system_hints": SEARCH_HINTS,
        "partial_query": {
            "id": new_uuid(),
            "author": { "role": "user" },
            "content": { "content_type": "text", "parts": [prompt] }
        },
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": { "app_name": "chatgpt.com" }
    })
}

/// `/backend-api/f/conversation` 的请求体（发起搜索）。
pub fn build_search_conversation_payload(prompt: &str, model: &str) -> Value {
    json!({
        "action": "next",
        "messages": [user_message(prompt)],
        "parent_message_id": "client-created-root",
        "model": model,
        "client_prepare_state": "success",
        "timezone_offset_min": -480,
        "timezone": "Asia/Shanghai",
        "conversation_mode": { "kind": "primary_assistant" },
        "enable_message_followups": true,
        "system_hints": [],
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "force_use_search": true,
        "client_reported_search_source": "conversation_composer_web_icon",
        "paragen_cot_summary_display_override": "allow",
        "force_parallel_switch": "auto",
        "client_contextual_info": {
            "is_dark_mode": false,
            "time_since_loaded": 36,
            "page_height": 925,
            "page_width": 886,
            "pixel_ratio": 2,
            "screen_height": 1440,
            "screen_width": 2560,
            "app_name": "chatgpt.com"
        }
    })
}

/// 来源条目。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchSource {
    /// 标题。
    pub title: String,
    /// URL（已去尾部标点，只留 http/https）。
    pub url: String,
    /// 摘要。
    pub snippet: String,
}

/// 搜索结果。
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    /// 会话 ID。
    pub conversation_id: String,
    /// 结束状态。
    pub status: String,
    /// 答案正文（注解已清洗）。
    pub answer: String,
    /// 来源（按 URL 去重）。
    pub sources: Vec<SearchSource>,
}

/// 去掉正文里的内部注解标记（U+E200 … U+E201）。
///
/// 上游用这两个私有区字符包裹引用信息：`url` 类型带「标题 + 链接」，
/// 其他类型（cite / turn…）只留可读的那一段。原样输出会是一堆方框。
pub fn clean_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('\u{E200}') {
        out.push_str(&rest[..start]);
        let after = &rest[start + '\u{E200}'.len_utf8()..];
        let Some(end) = after.find('\u{E201}') else {
            // 没有收尾标记：丢弃到结尾，避免把半个标记吐出去
            return out;
        };
        let body = &after[..end];
        out.push_str(&readable_annotation(body));
        rest = &after[end + '\u{E201}'.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// 把一条注解正文变成人可读的片段。
fn readable_annotation(body: &str) -> String {
    let fields: Vec<&str> = body.split('\u{E202}').map(str::trim).collect();
    let kind = fields.first().copied().unwrap_or_default().to_ascii_lowercase();
    let data: Vec<&str> = fields.iter().skip(1).copied().collect();
    if kind == "url" {
        let label = data.first().copied().unwrap_or_default();
        let url = data.get(1).copied().unwrap_or_default();
        if url.starts_with("http") {
            return if label.is_empty() {
                url.to_string()
            } else {
                format!("{label} ({url})")
            };
        }
        return label.to_string();
    }
    // cite / turn / source 之类：取第一段非编号、非关键字的可读文字
    data.iter()
        .map(|item| item.trim())
        .find(|item| {
            !item.is_empty()
                && !item.chars().all(|c| c.is_ascii_digit())
                && !item.to_ascii_lowercase().starts_with("source")
                && !item.to_ascii_lowercase().starts_with("turn")
        })
        .unwrap_or_default()
        .to_string()
}

/// 解析一份会话详情，取出答案与来源。
///
/// 来源形状不固定（不同轮次字段不同），所以不按固定 schema 解，
/// 而是递归找所有带 url 的对象，按 URL 去重。
pub fn parse_search_result(payload: &Value) -> SearchResult {
    let detail = crate::conversation::parse_conversation_detail(payload);
    let answer = crate::conversation::latest_assistant_message(&detail)
        .map(|message| clean_annotations(&message.text))
        .unwrap_or_default();
    let status = crate::conversation::latest_assistant_message(&detail)
        .map(|message| message.status.clone())
        .unwrap_or_default();

    let mut sources: Vec<SearchSource> = Vec::new();
    collect_sources(payload, &mut sources);
    SearchResult {
        conversation_id: detail.id,
        status,
        answer: answer.trim().to_string(),
        sources,
    }
}

fn collect_sources(value: &Value, out: &mut Vec<SearchSource>) {
    match value {
        Value::Object(map) => {
            let url = map
                .get("url")
                .or_else(|| map.get("link"))
                .or_else(|| map.get("source_url"))
                .and_then(Value::as_str)
                .map(canonical_url)
                .unwrap_or_default();
            if !url.is_empty() && !out.iter().any(|item| item.url == url) {
                let pick = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|key| map.get(*key).and_then(Value::as_str))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                out.push(SearchSource {
                    title: pick(&["title", "name", "source"]),
                    url,
                    snippet: pick(&["snippet", "text", "description"]),
                });
            }
            for nested in map.values() {
                collect_sources(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_sources(item, out);
            }
        }
        _ => {}
    }
}

/// 去掉纯追踪用的查询参数。命中这些前缀/名字的参数一律丢弃。
const TRACKING_PARAMS: [&str; 8] = [
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "linkId",
    "gclid",
    "fbclid",
];

/// 规范化 URL：用于**去重**与对外展示。
///
/// 上游同一篇来源会同时给出干净链接和带跟踪参数的链接，
/// 不归一化就会出现两份同样的来源。这里丢掉 fragment 与追踪参数，
/// 其余查询参数原样保留（可能承载页面身份）。
pub fn canonical_url(raw: &str) -> String {
    let trimmed = clean_url(raw);
    if trimmed.is_empty() {
        return trimmed;
    }
    let (base, fragment) = match trimmed.split_once('#') {
        Some((head, _)) => (head, ()),
        None => (trimmed.as_str(), ()),
    };
    let _ = fragment;
    let (path_part, query) = match base.split_once('?') {
        Some((head, tail)) => (head, tail),
        None => (base, ""),
    };
    if query.is_empty() {
        return path_part.to_string();
    }
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or_default();
            !TRACKING_PARAMS.iter().any(|tracked| name.eq_ignore_ascii_case(tracked))
        })
        .collect();
    if kept.is_empty() {
        path_part.to_string()
    } else {
        format!("{path_part}?{}", kept.join("&"))
    }
}

/// 只接受 http(s)，并去掉正文里常见的尾部标点。
fn clean_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches(['.', ',', ';', ')', ']', '。', '，', '）']);
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Value {
        json!({
            "conversation_id": "conv-1",
            "mapping": {
                "a": { "message": {
                    "id": "m1", "author": { "role": "user" }, "create_time": 1.0,
                    "content": { "content_type": "text", "parts": ["今天北京天气"] }
                } },
                "b": { "message": {
                    "id": "m2", "author": { "role": "assistant" }, "create_time": 2.0,
                    "content": { "content_type": "text", "parts": [
                        "北京今天晴，18℃\u{E200}url\u{E202}中国天气网\u{E202}https://weather.example/bj\u{E201}。"
                    ] },
                    "metadata": {
                        "finish_details": { "type": "finished_successfully" },
                        "search_result": [
                            { "title": "中国天气网", "url": "https://weather.example/bj", "snippet": "北京晴" },
                            { "title": "另一个来源", "url": "https://other.example/x." }
                        ]
                    }
                } }
            }
        })
    }

    #[test]
    fn prepare_and_conversation_payloads_carry_search_hints() {
        let prepare = build_search_prepare_payload("q", "gpt-5-5");
        assert_eq!(prepare["system_hints"], json!(["search"]));
        assert_eq!(prepare["partial_query"]["content"]["parts"], json!(["q"]));

        let conversation = build_search_conversation_payload("q", "gpt-5-5");
        assert_eq!(conversation["force_use_search"], json!(true));
        assert_eq!(conversation["messages"][0]["metadata"]["system_hints"], json!(["search"]));
        assert_eq!(conversation["messages"][0]["content"]["parts"], json!(["q"]));
    }

    #[test]
    fn annotation_url_becomes_readable() {
        let cleaned = clean_annotations("见\u{E200}url\u{E202}中国天气网\u{E202}https://a.example/x\u{E201}。");
        assert_eq!(cleaned, "见中国天气网 (https://a.example/x)。");
    }

    #[test]
    fn annotation_without_url_keeps_readable_part() {
        let cleaned = clean_annotations("答案\u{E200}cite\u{E202}某来源\u{E201}完");
        assert_eq!(cleaned, "答案某来源完");
    }

    #[test]
    fn unterminated_annotation_is_dropped() {
        let cleaned = clean_annotations("正文\u{E200}url\u{E202}半截");
        assert_eq!(cleaned, "正文", "没有收尾标记时不该把半个标记吐出去");
    }

    #[test]
    fn parses_answer_and_deduped_sources() {
        let result = parse_search_result(&payload());
        assert_eq!(result.conversation_id, "conv-1");
        assert_eq!(result.status, "finished_successfully");
        assert!(result.answer.contains("北京今天晴"), "实际: {}", result.answer);
        assert!(!result.answer.contains('\u{E200}'), "注解必须被清掉");
        assert_eq!(result.sources.len(), 2, "两个不同 URL");
        assert_eq!(result.sources[0].url, "https://weather.example/bj");
        assert_eq!(result.sources[0].title, "中国天气网");
        assert_eq!(result.sources[1].url, "https://other.example/x", "尾部句点应去掉");
    }

    #[test]
    fn ignores_non_http_urls() {
        let value = json!({ "x": { "url": "sediment://file_1" }, "y": { "link": "ftp://a/b" } });
        let result = parse_search_result(&value);
        assert!(result.sources.is_empty(), "只接受 http(s)");
    }

    #[test]
    fn empty_payload_yields_empty_result() {
        let result = parse_search_result(&json!({}));
        assert!(result.answer.is_empty());
        assert!(result.sources.is_empty());
    }

    #[test]
    fn canonical_url_strips_tracking_params() {
        let canonical = canonical_url("https://a.example/x?utm_source=chatgpt.com");
        assert_eq!(canonical, "https://a.example/x");
        let canonical = canonical_url("https://a.example/x?id=7&utm_medium=cpc");
        assert_eq!(canonical, "https://a.example/x?id=7", "业务参数要保留");
        let canonical = canonical_url("https://a.example/x#frag");
        assert_eq!(canonical, "https://a.example/x");
    }

    #[test]
    fn same_page_with_and_without_tracking_is_one_source() {
        let value = json!({
            "a": { "url": "https://weather.example/bj" },
            "b": { "url": "https://weather.example/bj?utm_source=chatgpt.com" }
        });
        let result = parse_search_result(&value);
        assert_eq!(result.sources.len(), 1, "同一页面只应保留一条");
        assert_eq!(result.sources[0].url, "https://weather.example/bj");
    }

    #[test]
    fn non_http_urls_are_rejected_by_canonical() {
        assert_eq!(canonical_url("sediment://file_1"), "");
        assert_eq!(canonical_url("ftp://a/b"), "");
    }

    /// 真实搜索响应的脱敏副本（2026-11-12 采集）。
    ///
    /// 结构来自上游，长文本已截断。用它锁住「真实响应能被解析出答案与来源」。
    fn real_fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/search_detail.json"))
            .expect("夹具应是合法 JSON")
    }

    #[test]
    fn parses_real_search_fixture() {
        let result = parse_search_result(&real_fixture());
        assert!(!result.answer.trim().is_empty(), "真实响应应能解出答案");
        assert!(
            !result.answer.contains('\u{E200}'),
            "答案里的注解必须被清掉"
        );
        assert!(!result.sources.is_empty(), "真实响应应带来源");
        for source in &result.sources {
            assert!(
                source.url.starts_with("http"),
                "来源 URL 必须是 http(s)：{}",
                source.url
            );
            assert!(
                !source.url.contains("utm_source"),
                "来源 URL 不该带追踪参数：{}",
                source.url
            );
        }
        let unique: std::collections::HashSet<&str> =
            result.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(unique.len(), result.sources.len(), "来源按 URL 去重");
    }
}

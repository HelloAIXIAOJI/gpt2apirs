//! 下载链路纯逻辑的测试。
//!
//! 使用 v3-capture 真实抓到的 conversation_id 与 sediment_id。

use gpt2apirs_protocol::{
    attachment_download_path, file_download_path, is_same_origin, parse_download_url,
    plan_download_targets, should_skip_file_id,
};
use serde_json::json;

/// 真实抓包中的 ID。
const REAL_CONVERSATION_ID: &str = "6af329ce-5a14-83ee-a906-4927ae2adff4";
const REAL_SEDIMENT_ID: &str = "file_0000000028a48209897e753c161fbf5a";

#[test]
fn attachment_path_matches_python() {
    // Python: f"/backend-api/conversation/{conversation_id}/attachment/{attachment_id}/download"
    let actual = attachment_download_path(REAL_CONVERSATION_ID, REAL_SEDIMENT_ID);
    let expected = format!(
        "/backend-api/conversation/{REAL_CONVERSATION_ID}/attachment/{REAL_SEDIMENT_ID}/download"
    );
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        "/backend-api/conversation/6af329ce-5a14-83ee-a906-4927ae2adff4/attachment/file_0000000028a48209897e753c161fbf5a/download"
    );
}

#[test]
fn file_path_matches_python() {
    // Python: f"/backend-api/files/{file_id}/download"
    let actual = file_download_path(REAL_SEDIMENT_ID);
    assert_eq!(
        actual,
        "/backend-api/files/file_0000000028a48209897e753c161fbf5a/download"
    );
}

#[test]
fn parse_download_url_prefers_download_url() {
    let body = json!({
        "download_url": "https://files.example/a.png?sig=1",
        "url": "https://files.example/b.png?sig=2"
    });
    assert_eq!(
        parse_download_url(&body).as_deref(),
        Some("https://files.example/a.png?sig=1")
    );
}

#[test]
fn parse_download_url_falls_back_to_url() {
    let body = json!({ "url": "https://files.example/c.png" });
    assert_eq!(
        parse_download_url(&body).as_deref(),
        Some("https://files.example/c.png")
    );
}

#[test]
fn parse_download_url_handles_empty_and_missing() {
    // 空字符串要跳过，继续用下一个字段
    let body = json!({ "download_url": "", "url": "https://files.example/d.png" });
    assert_eq!(
        parse_download_url(&body).as_deref(),
        Some("https://files.example/d.png")
    );
    // 全空
    assert_eq!(parse_download_url(&json!({})), None);
    assert_eq!(parse_download_url(&json!({ "download_url": "" })), None);
    // 只有空白
    assert_eq!(parse_download_url(&json!({ "download_url": "   " })), None);
    // 非字符串
    assert_eq!(parse_download_url(&json!({ "download_url": 123 })), None);
}

#[test]
fn same_origin_detection() {
    let base = "https://chatgpt.com";
    // 同源
    assert!(is_same_origin(base, "https://chatgpt.com/backend-api/x"));
    // 相对路径
    assert!(is_same_origin(base, "/backend-api/x"));
    // 跨域 CDN
    assert!(!is_same_origin(base, "https://cdn.oaistatic.com/x.png"));
    assert!(!is_same_origin(base, "https://files.oaiusercontent.com/x.png?sig=1"));
    // 大小写不敏感
    assert!(is_same_origin("https://ChatGPT.com", "https://chatgpt.COM/x"));
}

#[test]
fn skip_file_upload_sentinel() {
    assert!(should_skip_file_id("file_upload"));
    assert!(!should_skip_file_id(REAL_SEDIMENT_ID));
}

#[test]
fn plan_targets_dedupes_and_skips() {
    let file_ids = vec![
        "file_upload".to_string(),
        REAL_SEDIMENT_ID.to_string(),
        REAL_SEDIMENT_ID.to_string(), // 重复
    ];
    let sediment_ids = vec![REAL_SEDIMENT_ID.to_string(), REAL_SEDIMENT_ID.to_string()];

    let (attachments, files) = plan_download_targets(REAL_CONVERSATION_ID, &file_ids, &sediment_ids);

    // file_upload 被跳过，只剩一个真实 ID，且去重
    assert_eq!(files, vec![REAL_SEDIMENT_ID.to_string()]);
    // sediment 去重
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].0, REAL_CONVERSATION_ID);
    assert_eq!(attachments[0].1, REAL_SEDIMENT_ID);
}

#[test]
fn plan_targets_requires_conversation_for_attachments() {
    let sediment_ids = vec![REAL_SEDIMENT_ID.to_string()];
    let (attachments, files) = plan_download_targets("", &[], &sediment_ids);
    // 没有 conversation_id 时不能解析 sediment
    assert!(attachments.is_empty());
    assert!(files.is_empty());
}

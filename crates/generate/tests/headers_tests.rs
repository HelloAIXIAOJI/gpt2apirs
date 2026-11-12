//! 头部构造的回归测试。
//!
//! 重点：不得在头部列表里手写 Content-Type。
//!
//! 曾经踩过的坑：`http::Client` 的 `.json(body)` 会写入 Content-Type，
//! 若头部列表里再带一个，reqwest 会 append 出
//! `Content-Type: application/json, application/json`，
//! 上游 FastAPI 解析失败，报：
//! `{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary"}]}`

use gpt2apirs_http::browser_headers;

/// 这是一个占位测试，真正被验证的是 `generate` 内部的私有函数；
/// 这里用公开的 `browser_headers` 守住「基础头里不含 Content-Type」。
#[test]
fn browser_headers_have_no_content_type() {
    let headers = browser_headers("test-agent");
    assert!(
        !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type")),
        "browser_headers 不应包含 Content-Type（交给 .json() 统一写入）"
    );
    // 但必须包含 User-Agent
    assert!(headers.iter().any(|(name, _)| *name == "User-Agent"));
}

/// 基础头不应重复。
#[test]
fn browser_headers_are_unique() {
    use std::collections::HashSet;
    let headers = browser_headers("test-agent");
    let mut seen = HashSet::new();
    for (name, _) in &headers {
        assert!(seen.insert(name.to_ascii_lowercase()), "重复的头: {name}");
    }
}

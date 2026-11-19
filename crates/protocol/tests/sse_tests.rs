//! SSE 解析与**真实抓包**的对比测试。
//!
//! `fixtures/real_image_sse.txt` 是用 `v3-capture` 从 chatgpt.com 真实抓取的
//! 生图 SSE 流（已脱敏：长 token 替换为占位符）。
//!
//! `sse_expected.json` 是从真实抓包中人工核对出的期望结果
//! （会话 ID、file id、sediment id、是否被拦、turn 用途）。

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SseExpected {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "fileIds")]
    file_ids: Vec<String>,
    #[serde(rename = "sedimentIds")]
    sediment_ids: Vec<String>,
    blocked: bool,
    #[serde(rename = "toolInvoked")]
    tool_invoked: Option<bool>,
    #[serde(rename = "turnUseCase")]
    turn_use_case: String,
}

fn expected() -> SseExpected {
    serde_json::from_str(include_str!("sse_expected.json"))
        .expect("sse_expected.json 解析失败")
}

/// 真实抓包的关键字段必须与 Python 实现完全一致。
#[test]
fn real_capture_matches_python() {
    let raw = include_str!("fixtures/real_image_sse.txt");
    let want = expected();

    let (events, state) = gpt2apirs_protocol::parse_conversation_stream(raw);

    assert_eq!(
        state.conversation_id, want.conversation_id,
        "conversation_id 不一致"
    );
    assert_eq!(state.file_ids, want.file_ids, "file_ids 不一致");
    assert_eq!(state.sediment_ids, want.sediment_ids, "sediment_ids 不一致");
    assert_eq!(state.blocked, want.blocked, "blocked 不一致");
    assert_eq!(state.tool_invoked, want.tool_invoked, "tool_invoked 不一致");
    assert_eq!(state.turn_use_case, want.turn_use_case, "turn_use_case 不一致");

    // 事件数：payload 数（含 [DONE]）
    let payload_count = gpt2apirs_protocol::iter_sse_payloads(raw).len();
    assert!(payload_count >= events.len(), "事件数不应超过 payload 数");
    assert!(!events.is_empty(), "应解析出事件");
}

/// 真实抓包必须能提取出 sediment id（这是拿图的唯一凭据）。
#[test]
fn real_capture_yields_sediment_id() {
    let raw = include_str!("fixtures/real_image_sse.txt");
    let (_, state) = gpt2apirs_protocol::parse_conversation_stream(raw);

    assert_eq!(state.sediment_ids.len(), 1, "应恰好 1 个 sediment id");
    let id = &state.sediment_ids[0];
    assert!(
        id.starts_with("file_00000000"),
        "sediment id 形态异常: {id}"
    );
    // file_ (5) + 00000000 (8) + 24 位十六进制 = 37
    assert_eq!(id.len(), 37, "sediment id 应为 37 字符: {id}");
}

/// 输入图不能被误判成输出图。
///
/// 真实流里 user 消息带 `system_hints: [\"picture_v2\"]` 且 content 是纯文本，
/// 没有 asset_pointer，因此不该产生 file_ids。此测试确认不会误收集。
#[test]
fn user_message_not_collected_as_output() {
    let user_event = r#"{"v":{"message":{"author":{"role":"user"},"content":{"content_type":"text","parts":["hi"]}}}}"#;
    let (fids, sids) = {
        let (a, b, c) = gpt2apirs_protocol::extract_conversation_ids(user_event);
        let _ = a;
        (b, c)
    };
    assert!(fids.is_empty(), "用户文本消息不应有 file_ids");
    assert!(sids.is_empty(), "用户文本消息不应有 sediment_ids");

    let mut state = gpt2apirs_protocol::ConversationState::default();
    gpt2apirs_protocol::update_conversation_state(&mut state, user_event);
    assert!(state.file_ids.is_empty(), "用户消息不应进入 file_ids");
}

/// 核心正则的行为校验。
#[test]
fn id_extraction_rules() {
    // sediment:// 与 file_00000000... 都应被识别
    let payload = r#"{"asset_pointer":"sediment://file_0000000028a48209897e753c161fbf5a"}"#;
    let (_, fids, sids) = gpt2apirs_protocol::extract_conversation_ids(payload);
    assert_eq!(fids, vec!["file_0000000028a48209897e753c161fbf5a"]);
    assert_eq!(sids, vec!["file_0000000028a48209897e753c161fbf5a"]);

    // file-service:// 前缀应被当作 ID 来源
    let p2 = r#"{"asset_pointer":"file-service://file-abc123XYZ"}"#;
    let (_, f2, _) = gpt2apirs_protocol::extract_conversation_ids(p2);
    assert_eq!(f2, vec!["file-abc123XYZ"]);

    // 非图片文件 ID（如 file_upload_business_upsell）不应被收集
    let p3 = r#"{"id":"file_upload_business_upsell"}"#;
    let (_, f3, s3) = gpt2apirs_protocol::extract_conversation_ids(p3);
    assert!(f3.is_empty(), "file_upload_* 不应被收集: {f3:?}");
    assert!(s3.is_empty());
}

/// `[DONE]` 终止解析。
#[test]
fn done_terminates_stream() {
    let raw = "data: {\"type\":\"a\"}\ndata: [DONE]\ndata: {\"type\":\"never\"}\n";
    let (events, _) = gpt2apirs_protocol::parse_conversation_stream(raw);
    assert_eq!(events.len(), 2, "应只处理到 [DONE]");
    assert_eq!(events[1].kind, gpt2apirs_protocol::ConversationEventKind::Done);
}

/// 上游只回文本（拒绝/解释）时，正文必须留下来。
///
/// 生图链路以前只抽 `file-service://` / `sediment://` 两种 id，assistant 说的话
/// 直接丢掉，于是「没出图」对外只能报一个空错误。
#[test]
fn assistant_text_is_kept_for_no_image_cases() {
    let payload = r#"{"v":{"message":{"author":{"role":"assistant"},"content":{"content_type":"text","parts":["抱歉，我无法生成这张图片。"]},"recipient":"all","channel":"final"}}}"#;
    let mut state = gpt2apirs_protocol::ConversationState::default();
    gpt2apirs_protocol::update_conversation_state(&mut state, payload);
    assert!(state.file_ids.is_empty());
    assert!(state.sediment_ids.is_empty());
    assert_eq!(state.text, "抱歉，我无法生成这张图片。");
}

/// 思维链 / 发给工具的 / 隐藏消息都不算「上游说的话」。
#[test]
fn hidden_or_tool_messages_do_not_become_text() {
    let mut state = gpt2apirs_protocol::ConversationState::default();
    for payload in [
        r#"{"v":{"message":{"author":{"role":"assistant"},"content":{"parts":["思维链"]},"channel":"analysis"}}}"#,
        r#"{"v":{"message":{"author":{"role":"assistant"},"content":{"parts":["发给工具"]},"recipient":"web"}}}"#,
        r#"{"v":{"message":{"author":{"role":"assistant"},"content":{"parts":["界面不显示"]},"metadata":{"is_visually_hidden_from_conversation":true}}}}"#,
    ] {
        gpt2apirs_protocol::update_conversation_state(&mut state, payload);
    }
    assert!(state.text.is_empty(), "不该收集到文本：{}", state.text);
}

/// 审核拦截标记要能被读到（生图链路据此判内容策略）。
#[test]
fn moderation_flag_is_recorded() {
    let payload = r#"{"type":"moderation","moderation_response":{"blocked":true}}"#;
    let mut state = gpt2apirs_protocol::ConversationState::default();
    gpt2apirs_protocol::update_conversation_state(&mut state, payload);
    assert!(state.blocked);
}

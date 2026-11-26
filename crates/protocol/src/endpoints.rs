//! 上游端点清单。
//!
//! 这里只放**路径常量**，不涉及 HTTP。
//!
//! # 为什么集中
//!
//! 上游路径散在各处时，一旦某个路径改名，grep 很难找全。
//! 集中成常量后，改一处、编译器帮你找出所有引用点。

/// 上游站点默认值。
pub const BASE_URL: &str = "https://chatgpt.com";

/// 覆盖上游站点用的环境变量（测试 seam）。
///
/// 生产不该设它：设了就等于把所有上游流量指向别处。cli 启动时会就此打 WARN。
pub const BASE_URL_ENV: &str = "GPT2APIRS_UPSTREAM_BASE";

/// 被环境变量覆盖后的上游站点；没有覆盖时为 `None`。
///
/// 启动方（cli）据此打告警：本 crate 不引日志依赖。
pub fn base_url_override() -> Option<&'static str> {
    static OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            let value = std::env::var(BASE_URL_ENV).ok()?;
            let trimmed = value.trim().trim_end_matches('/');
            if trimmed.is_empty() || trimmed == BASE_URL {
                return None;
            }
            Some(trimmed.to_string())
        })
        .as_deref()
}

/// 实际使用的上游站点。
///
/// 默认就是 [`BASE_URL`]；只有测试会通过环境变量把它指到本地 mock 服务。
/// 取值在进程内缓存一次 —— 环境变量中途改动不生效，
/// 否则同一请求的前后两段可能打到不同主机。
pub fn base_url() -> &'static str {
    base_url_override().unwrap_or(BASE_URL)
}

/// 拼一条完整的上游地址。
pub fn url(path: &str) -> String {
    format!("{}{}", base_url(), path)
}

/// 首页（bootstrap，取 PoW 脚本与 data-build）。
pub const HOME: &str = "/";

/// 已登录 sentinel 前置参数。
pub const SENTINEL_PREPARE: &str = "/backend-api/sentinel/chat-requirements/prepare";
/// 已登录 sentinel 最终确认。
pub const SENTINEL_FINALIZE: &str = "/backend-api/sentinel/chat-requirements/finalize";

/// 匿名 sentinel 前置参数。
pub const SENTINEL_PREPARE_ANON: &str = "/backend-anon/sentinel/chat-requirements/prepare";
/// 匿名 sentinel 最终确认。
pub const SENTINEL_FINALIZE_ANON: &str = "/backend-anon/sentinel/chat-requirements/finalize";

/// 会话准备（取 conduit_token）。
pub const CONVERSATION_PREPARE: &str = "/backend-api/f/conversation/prepare";
/// 发起会话（SSE 流）。
pub const CONVERSATION: &str = "/backend-api/f/conversation";

/// 账号信息。
pub const ME: &str = "/backend-api/me";
/// 会话初始化（返回 limits_progress，用于算图片配额）。
pub const CONVERSATION_INIT: &str = "/backend-api/conversation/init";
/// 账号检查（返回 plan_type）。
pub const ACCOUNTS_CHECK: &str = "/backend-api/accounts/check/v4-2023-04-27";
/// 模型列表（已登录）。
pub const MODELS: &str = "/backend-api/models";
/// 模型列表（匿名）。
pub const MODELS_ANON: &str = "/backend-anon/models";

/// 文件上传元数据申请（POST，返回 file_id + upload_url）。
pub const FILE_UPLOAD: &str = "/backend-api/files";

/// 文件上传完成确认。
pub fn file_uploaded(file_id: &str) -> String {
    format!("/backend-api/files/{file_id}/uploaded")
}

/// 文件下载地址解析。
pub fn file_download(file_id: &str) -> String {
    format!("/backend-api/files/{file_id}/download")
}

/// 附件（sediment）下载地址解析。
pub fn attachment_download(conversation_id: &str, attachment_id: &str) -> String {
    format!("/backend-api/conversation/{conversation_id}/attachment/{attachment_id}/download")
}

/// 会话详情（含消息树）。
///
/// 注意是**复数** `conversations` 且必须带 `num_turns`：
/// 光用 `/backend-api/conversation/{id}` 会 404（实测）。
pub fn conversation_detail(conversation_id: &str) -> String {
    format!("/backend-api/conversations/{conversation_id}?num_turns=10&include_has_versions=true")
}

/// 会话的单数路径（附件下载等地方用它拼子路径）。
///
/// 直接 GET 这个路径会 404 —— 要拿详情请用 [`conversation_detail`]。
pub fn conversation(conversation_id: &str) -> String {
    format!("/backend-api/conversation/{conversation_id}")
}

/// 删除会话。
///
/// 注意路径里多一段 `id`：实测 `DELETE /backend-api/conversation/{id}` 是 405，
/// 只有 `conversation/id/{id}` 认 DELETE。写错的表现是「删除永远失败」。
pub fn conversation_delete(conversation_id: &str) -> String {
    format!("/backend-api/conversation/id/{conversation_id}")
}

/// 会话列表（分页，按更新时间倒序）。
pub fn conversations_page(offset: i64, limit: i64) -> String {
    format!(
        "/backend-api/conversations?offset={}&limit={}&order=updated",
        offset.max(0),
        limit.clamp(1, 100)
    )
}

/// 已保存的记忆（GET 列列表）。
pub const MEMORIES: &str = "/backend-api/memories";

/// 语音转写：multipart 上传音频，返回 `{text}`。
pub const TRANSCRIBE: &str = "/backend-api/transcribe";

/// 语音合成。**只能合成已存在的消息**，所以要带 conversation_id + message_id。
pub fn synthesize(conversation_id: &str, message_id: &str, voice: &str, format: &str) -> String {
    format!(
        "/backend-api/synthesize?conversation_id={conversation_id}&message_id={message_id}&voice={voice}&format={format}"
    )
}

/// 账号级设置：记忆总开关（`m3m`）就在这个响应里。
pub const USER_SETTINGS: &str = "/backend-api/settings/user";

/// 清空已保存的记忆（不动开关）。
pub const MEMORY_CLEAR: &str = "/backend-api/settings/clear_account_user_memory";

/// 改记忆开关。上游要 PATCH，参数全在 query 里。
pub fn memory_setting(enabled: bool) -> String {
    format!("/backend-api/settings/account_user_setting?feature=m3m&value={enabled}")
}

/// 删除一条记忆。
pub fn memory_delete(memory_id: &str) -> String {
    format!("/backend-api/memories/{memory_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_start_with_slash() {
        for path in [
            HOME,
            SENTINEL_PREPARE,
            SENTINEL_FINALIZE,
            CONVERSATION_PREPARE,
            CONVERSATION,
            ME,
            MODELS,
            CONVERSATION_INIT,
            ACCOUNTS_CHECK,
            FILE_UPLOAD,
            MEMORIES,
        ] {
            assert!(path.starts_with('/'), "路径应以 / 开头: {path}");
        }
    }

    #[test]
    fn conversation_delete_uses_id_segment() {
        // 上游只认 conversation/id/{id}：漏了 id 段会 405，删除永远失败
        assert_eq!(
            conversation_delete("abc"),
            "/backend-api/conversation/id/abc"
        );
    }

    #[test]
    fn conversations_page_carries_paging() {
        assert_eq!(
            conversations_page(0, 28),
            "/backend-api/conversations?offset=0&limit=28&order=updated"
        );
        // 负数/超限都要被兜住：上游对 limit=0 会回空列表，看着像「没数据」
        assert!(conversations_page(-5, 0).contains("offset=0&limit=1"));
        assert!(conversations_page(0, 500).contains("limit=100"));
    }

    #[test]
    fn anon_paths_use_anon_prefix() {
        assert!(SENTINEL_PREPARE_ANON.starts_with("/backend-anon/"));
        assert!(SENTINEL_FINALIZE_ANON.starts_with("/backend-anon/"));
        assert!(MODELS_ANON.starts_with("/backend-anon/"));
    }

    #[test]
    fn dynamic_paths_interpolate() {
        assert_eq!(
            file_download("file_abc"),
            "/backend-api/files/file_abc/download"
        );
        assert_eq!(
            file_uploaded("file_abc"),
            "/backend-api/files/file_abc/uploaded"
        );
        assert_eq!(
            attachment_download("conv-1", "file_x"),
            "/backend-api/conversation/conv-1/attachment/file_x/download"
        );
        assert_eq!(conversation("conv-1"), "/backend-api/conversation/conv-1");
    }
}

#[cfg(test)]
mod base_url_tests {
    use super::*;

    /// 没有环境变量时就是默认站点，且 `url()` 只是拼接。
    ///
    /// **不要**在这里设 `BASE_URL_ENV`：取值在进程内只算一次，
    /// 同一进程里的其它用例会读到被污染的值。覆盖行为由 mock 上游的集成测试负责。
    #[test]
    fn defaults_to_the_real_upstream() {
        assert_eq!(base_url_override(), None);
        assert_eq!(base_url(), BASE_URL);
        assert_eq!(url("/backend-api/me"), "https://chatgpt.com/backend-api/me");
        assert_eq!(url(""), BASE_URL);
    }
}

//! 上游端点清单。
//!
//! 这里只放**路径常量**，不涉及 HTTP。
//!
//! # 为什么集中
//!
//! 上游路径散在各处时，一旦某个路径改名，grep 很难找全。
//! 集中成常量后，改一处、编译器帮你找出所有引用点。

/// 上游站点。
pub const BASE_URL: &str = "https://chatgpt.com";

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

/// 会话详情。
pub fn conversation(conversation_id: &str) -> String {
    format!("/backend-api/conversation/{conversation_id}")
}

/// 删除会话。
pub fn conversation_delete(conversation_id: &str) -> String {
    format!("/backend-api/conversation/{conversation_id}")
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
        ] {
            assert!(path.starts_with('/'), "路径应以 / 开头: {path}");
        }
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

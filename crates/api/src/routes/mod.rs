//! `/v1/*` 与 `/api/*` 路由。

pub mod accounts;
pub mod backup;
pub mod buckets;
pub mod conversations;
pub mod admin;
pub mod image_common;
pub mod image_flow;
pub mod image_tasks;
pub mod images;
pub mod dashboard;
pub mod logs;
pub mod memories;
pub mod rich_text;
pub mod proxy;
pub mod settings;
pub mod text_cache;

#[cfg(test)]
pub mod test_support;
pub mod text_common;
pub mod v1_audio;
pub mod v1_chat;
pub mod v1_image_edits;
pub mod v1_messages;
pub mod v1_responses;
pub mod v1_search;
pub mod v1_images;
pub mod v1_models;

pub use accounts::{
    AccountListResponse, AccountView, BatchRequest, BatchResponse, DeleteAccountsRequest,
    ExportRequest, ImportItem, ImportResponse, ListQuery, OAuthFinishRequest, OAuthStartRequest,
    TestRequest, TestResponse, UpdateAccountRequest, create_accounts, delete_account,
    delete_accounts,
    export_accounts, get_account, get_operation, list_accounts, oauth_finish, oauth_start,
    refresh_tokens,
    stats, sync_accounts, test_account, update_account,
};
/// 文本类 SSE 的空闲心跳。
///
/// 上游「思考」期间实测有 10 秒左右没有任何可见文本（思维链被我们过滤掉了），
/// 这段时间我们一个字节都不发：客户端以为卡死，中间的 CF 之类的代理还会攒包，
/// 把首字节一路推迟到十几秒。
///
/// axum 的 KeepAlive 只在**空闲**时发 `: keepalive` 注释帧（有真实事件就重置计时），
/// 注释帧按 SSE 规范会被客户端忽略，不影响正文。
pub(crate) fn sse_keep_alive() -> axum::response::sse::KeepAlive {
    axum::response::sse::KeepAlive::new()
        .interval(std::time::Duration::from_secs(2))
        .text("keepalive")
}

pub use v1_image_edits::edit_images;
pub use v1_images::{ImageGenerationRequest, generate};
pub use v1_models::{get_model, list_models};

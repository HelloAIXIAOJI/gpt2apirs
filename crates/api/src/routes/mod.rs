//! `/v1/*` 与 `/api/*` 路由。

pub mod accounts;
pub mod backup;
pub mod admin;
pub mod image_common;
pub mod image_flow;
pub mod image_tasks;
pub mod images;
pub mod logs;
pub mod rich_text;
pub mod proxy;
pub mod settings;
pub mod text_cache;

#[cfg(test)]
pub mod test_support;
pub mod text_common;
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
pub use v1_image_edits::edit_images;
pub use v1_images::{ImageGenerationRequest, generate};
pub use v1_models::{get_model, list_models};

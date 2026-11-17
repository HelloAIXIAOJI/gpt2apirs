//! 账号池管理接口（`/api/accounts*`）。
//!
//! 对照 `docs/account-pool-features.md` 的按钮清单实现服务端部分。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::Request;
use chrono::{DateTime, Utc};
use gpt2apirs_account::{
    Account, AccountFilter, AccountPatch, AccountStatus, CredentialAvailability, OperationKind,
    OperationProgress, Plan, PoolError, Purpose, SelectionFilter, Source,
    build_export, build_token_txt, parse_any, parse_json, project,
};
use gpt2apirs_core::{AppError, Deadline};
use gpt2apirs_generate::{ImageRequest, default_request_deadline, generate_images};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, ApiError>;

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

fn pool_error(error: PoolError) -> ApiError {
    match error {
        PoolError::NoCandidate { reason } => ApiError::admin(AppError::ServiceUnavailable {
            message: format!("无可用账号（{reason}），请先通过 POST /api/accounts 导入"),
            code: "no_available_account".to_string(),
        }),
        PoolError::DeadlineExceeded => ApiError::admin(AppError::ServiceUnavailable {
            message: "等待账号槽位超时，请稍后重试".to_string(),
            code: "account_pool_timeout".to_string(),
        }),
        other => internal(other),
    }
}

/// 账号视图（含派生的凭证状态，只读）。
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    /// 账号 ID。
    pub id: Uuid,
    /// 邮箱。
    pub email: Option<String>,
    /// 套餐。
    pub plan: &'static str,
    /// 来源。
    pub source: &'static str,
    /// 业务状态。
    pub status: &'static str,
    /// 业务状态中文。
    pub status_label: &'static str,
    /// 剩余配额。
    pub quota: i32,
    /// 配额是否被远端确认。
    pub quota_confirmed: bool,
    /// 限流恢复时间。
    pub restore_at: Option<DateTime<Utc>>,
    /// 代理。
    pub proxy: String,
    /// 成功次数。
    pub success_count: i64,
    /// 失败次数。
    pub fail_count: i64,
    /// 连续失效次数。
    pub invalid_count: i32,
    /// 是否可刷新 access token（有 refresh_token）。
    pub can_refresh_access_token: bool,
    /// 凭证可用性。
    pub credential: CredentialView,
    /// 最近错误。
    pub last_error: Option<String>,
    /// 最近校验时间。
    pub last_checked_at: Option<DateTime<Utc>>,
    /// 最近刷新时间。
    pub last_refresh_at: Option<DateTime<Utc>>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 开关动作的建议文案（前端按钮用）。
    pub enabled_action_label: &'static str,
}

/// 凭证视图。
#[derive(Debug, Clone, Serialize)]
pub struct CredentialView {
    /// access token 状态。
    pub access: &'static str,
    /// access token 状态中文。
    pub access_label: &'static str,
    /// refresh token 状态。
    pub refresh: &'static str,
    /// refresh token 状态中文。
    pub refresh_label: &'static str,
    /// 综合可用性。
    pub usable: &'static str,
    /// 综合可用性中文。
    pub usable_label: &'static str,
    /// access token 到期时间。
    pub expires_at: Option<DateTime<Utc>>,
    /// 距到期秒数。
    pub expires_in_secs: Option<i64>,
}

impl CredentialView {
    fn from_account(account: &Account, now: DateTime<Utc>) -> Self {
        let CredentialAvailability {
            access,
            refresh,
            usable,
            expires_at,
            ..
        } = project(
            &account.access_token,
            account.refresh_token.as_deref().unwrap_or(""),
            account.status == AccountStatus::Abnormal,
            false,
            now,
        );
        Self {
            access: access_name(access),
            access_label: access.label(),
            refresh: refresh_name(refresh),
            refresh_label: refresh.label(),
            usable: usable_name(usable),
            usable_label: usable.label(),
            expires_at,
            expires_in_secs: expires_at.map(|exp| (exp - now).num_seconds()),
        }
    }
}

fn access_name(value: gpt2apirs_account::AccessTokenStatus) -> &'static str {
    use gpt2apirs_account::AccessTokenStatus::*;
    match value {
        Valid => "valid",
        Expiring => "expiring",
        Invalid => "invalid",
    }
}

fn refresh_name(value: gpt2apirs_account::RefreshTokenStatus) -> &'static str {
    use gpt2apirs_account::RefreshTokenStatus::*;
    match value {
        Valid => "valid",
        Missing => "missing",
        Invalid => "invalid",
    }
}

fn usable_name(value: gpt2apirs_account::Availability) -> &'static str {
    use gpt2apirs_account::Availability::*;
    match value {
        Usable => "usable",
        Recoverable => "recoverable",
        Unavailable => "unavailable",
    }
}

impl AccountView {
    fn from_account(account: Account) -> Self {
        let now = Utc::now();
        let credential = CredentialView::from_account(&account, now);
        Self {
            id: account.id,
            email: account.email,
            plan: account.plan.as_str(),
            source: account.source.as_str(),
            status: account.status.as_str(),
            status_label: account.status.label(),
            quota: account.quota,
            quota_confirmed: account.quota_confirmed,
            restore_at: account.restore_at,
            proxy: account.proxy,
            success_count: account.success_count,
            fail_count: account.fail_count,
            invalid_count: account.invalid_count,
            can_refresh_access_token: account
                .refresh_token
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty()),
            enabled_action_label: if account.status == AccountStatus::Disabled {
                "启用账号"
            } else {
                "禁用账号"
            },
            credential,
            last_error: account.last_error,
            last_checked_at: account.last_checked_at,
            last_refresh_at: account.last_refresh_at,
            created_at: account.created_at,
        }
    }
}

// 列表

/// 列表查询参数。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 状态过滤。
    pub status: Option<String>,
    /// 套餐过滤。
    pub plan: Option<String>,
    /// 来源过滤。
    pub source: Option<String>,
    /// 关键词。
    pub keyword: Option<String>,
    /// 代理分组过滤。
    pub group: Option<Uuid>,
    /// 页码（从 1 开始）。
    pub page: Option<i64>,
    /// 每页数量。
    pub page_size: Option<i64>,
}

/// 列表响应。
#[derive(Debug, Serialize)]
pub struct AccountListResponse {
    /// 当前页数据。
    pub items: Vec<AccountView>,
    /// 总条数。
    pub total: i64,
    /// 页码。
    pub page: i64,
    /// 每页数量。
    pub page_size: i64,
}

/// `GET /api/accounts`
pub async fn list_accounts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<AccountListResponse> {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 500);
    let status = match query.status.as_deref() {
        None => None,
        Some(raw) => Some(AccountStatus::parse(raw).ok_or_else(|| {
            ApiError::admin(AppError::invalid(format!("status 取值非法：{raw}")))
        })?),
    };
    let source = match query.source.as_deref() {
        None => None,
        Some(raw) => Some(Source::parse(raw).ok_or_else(|| {
            ApiError::admin(AppError::invalid(format!("source 取值非法：{raw}")))
        })?),
    };
    let filter = AccountFilter {
        status,
        plan: query.plan.as_deref().map(Plan::parse),
        source,
        keyword: query.keyword.clone(),
        offset: (page - 1) * page_size,
        limit: page_size,
    };

    let total = state.pool.repo().count(&filter).await.map_err(internal)?;
    let accounts = state.pool.repo().list(&filter).await.map_err(internal)?;
    Ok(Json(AccountListResponse {
        items: accounts.into_iter().map(AccountView::from_account).collect(),
        total,
        page,
        page_size,
    }))
}

/// `GET /api/accounts/{id}` —— 单个账号详情。
///
/// 与列表共用 [`AccountView`]，所以字段口径不会分叉；不存在时 404 而不是空对象。
pub async fn get_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<AccountView> {
    let account = state.pool.repo().get_by_id(id).await.map_err(internal)?;
    match account {
        Some(account) => Ok(Json(AccountView::from_account(account))),
        None => Err(ApiError::admin(AppError::not_found_coded(
            format!("账号不存在: {id}"),
            "account_not_found",
        ))),
    }
}

// 统计

/// `GET /api/accounts/stats`
pub async fn stats(State(state): State<Arc<AppState>>) -> ApiResult<Value> {
    let stats = state.pool.stats().await.map_err(internal)?;
    Ok(Json(serde_json::to_value(stats).unwrap_or(Value::Null)))
}

// 导入

/// 导入响应。
#[derive(Debug, Serialize)]
pub struct ImportResponse {
    /// 新增数量。
    pub added: usize,
    /// 合并数量（token 已存在，但补齐了 refresh_token / id_token / email 等）。
    pub merged: usize,
    /// 跳过数量（token 已存在且没有新字段可补）。
    pub skipped: usize,
    /// 逐条结果。
    pub items: Vec<ImportItem>,
    /// OAuth 流程自动触发的配额同步操作 ID（只有 OAuth 会带）。
    ///
    /// 单账号导入顺手同步一次很便宜；批量导入不自动做，
    /// 否则 100 个账号会立刻打 100 次上游。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_operation_id: Option<Uuid>,
}

/// 单条导入结果。
#[derive(Debug, Serialize)]
pub struct ImportItem {
    /// 账号标识（邮箱或 ID）。
    pub label: String,
    /// 结果：`added`（新增）/ `merged`（已存在，补齐了 refresh_token 等字段）/ `skipped`。
    pub outcome: &'static str,
    /// 附加说明。
    pub reason: Option<String>,
}

/// `POST /api/accounts`
///
/// 接受三种 body：
/// - 字符串：逐行 token 或 JSON 文本
/// - 数组：账号对象数组
/// - 对象：`{"content": "..."}` 或单个账号对象
pub async fn create_accounts(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> ApiResult<ImportResponse> {
    let bytes = axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .map_err(|error| internal(format!("读取请求体失败: {error}")))?;

    let new_accounts = parse_import_body(&bytes)?;
    if new_accounts.is_empty() {
        return Err(ApiError::admin(AppError::invalid(
            "没有解析出任何账号，请检查 access_token 字段",
        )));
    }

    let report = state
        .pool
        .repo()
        .upsert_many(&new_accounts)
        .await
        .map_err(internal)?;
    // 导入后立刻重载快照，让新账号马上可被选中
    let _ = state.pool.reload().await;

    let mut items: Vec<ImportItem> = report
        .inserted
        .iter()
        .map(|account| ImportItem {
            label: account.label(),
            outcome: "added",
            reason: None,
        })
        .collect();
    items.extend(report.merged.iter().map(|account| ImportItem {
        label: account.label(),
        outcome: "merged",
        reason: Some("已存在，合并了新提供的字段".to_string()),
    }));

    Ok(Json(ImportResponse {
        added: report.added(),
        merged: report.merged_count(),
        skipped: report.skipped_blank,
        items,
        // 批量导入不自动同步配额：账号多时会瞬间打出大量上游请求
        sync_operation_id: None,
    }))
}

fn parse_import_body(bytes: &[u8]) -> Result<Vec<gpt2apirs_account::NewAccount>, ApiError> {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        // 不是 JSON：按纯文本处理
        return parse_any(&text).map_err(|error| {
            ApiError::admin(AppError::invalid(format!("导入内容无法解析: {error}")))
        });
    };
    match &value {
        Value::String(content) => parse_any(content),
        Value::Array(_) => parse_json(&value.to_string()),
        Value::Object(map) => {
            if let Some(content) = map.get("content").and_then(Value::as_str) {
                parse_any(content)
            } else if let Some(items) = map.get("items") {
                parse_json(&items.to_string())
            } else {
                parse_json(&value.to_string())
            }
        }
        _ => Err(gpt2apirs_account::ImportParseError::NoAccounts),
    }
    .map_err(|error| {
        ApiError::admin(AppError::invalid(format!("导入内容无法解析: {error}")))
    })
}

// 更新 / 删除

/// 更新请求。
#[derive(Debug, Deserialize)]
pub struct UpdateAccountRequest {
    /// 账号 ID。
    pub id: Uuid,
    /// 新状态。
    pub status: Option<String>,
    /// 新套餐。
    pub plan: Option<String>,
    /// 新邮箱。
    pub email: Option<String>,
    /// 新代理。
    pub proxy: Option<String>,
    /// 手动校正剩余配额。
    ///
    /// 上游配额偶尔读到不准确的 0（例如限流刚恢复），管理面需要手工纠正；
    /// 允许直接改 `quota`：同步失败时可手工修正。
    pub quota: Option<i32>,
    /// 与 `quota` 配套：标记该配额是否已由人工确认。
    pub quota_confirmed: Option<bool>,
}

/// `POST /api/accounts/update`
pub async fn update_account(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateAccountRequest>,
) -> ApiResult<AccountView> {
    let mut patch = AccountPatch::default();
    if let Some(status) = body.status.as_deref() {
        let parsed = match status.trim().to_ascii_lowercase().as_str() {
            "normal" | "enable" | "enabled" => AccountStatus::Normal,
            "limited" | "limit" => AccountStatus::Limited,
            "abnormal" | "error" => AccountStatus::Abnormal,
            "disabled" | "disable" => AccountStatus::Disabled,
            other => {
                return Err(ApiError::admin(AppError::invalid_param(
                    format!("未知状态 {other:?}，可选 normal/limited/abnormal/disabled"),
                    "status",
                )));
            }
        };
        patch.status = Some(parsed);
    }
    patch.plan = body.plan.as_deref().map(Plan::parse);
    patch.email = body.email.clone();
    patch.proxy = body.proxy.clone();
    patch.quota = body.quota.map(|value| value.max(0));
    patch.quota_confirmed = body.quota_confirmed;

    let updated = state
        .pool
        .repo()
        .patch(body.id, &patch)
        .await
        .map_err(|error| match error {
            gpt2apirs_account::AccountError::NotFound(id) => {
                ApiError::admin(AppError::not_found(format!("账号不存在: {id}")))
            }
            other => internal(other),
        })?;
    let _ = state.pool.reload().await;
    Ok(Json(AccountView::from_account(updated)))
}

/// 删除请求。
#[derive(Debug, Deserialize)]
pub struct DeleteAccountsRequest {
    /// 要删除的账号 ID。
    pub ids: Vec<Uuid>,
}

/// `DELETE /api/accounts` —— 批量，只认请求体里的 ids。
///
/// 没有 body 时**不要**退化成「删光」：那是数据丢失，不是便利。
pub async fn delete_accounts(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DeleteAccountsRequest>,
) -> ApiResult<Value> {
    if body.ids.is_empty() {
        return Err(ApiError::admin(AppError::invalid_param(
            "ids 不能为空",
            "ids",
        )));
    }
    let removed = state
        .pool
        .repo()
        .delete_many(&body.ids)
        .await
        .map_err(internal)?;
    let _ = state.pool.reload().await;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

/// `DELETE /api/accounts/{id}` —— 单个，只认路径里的 id。
///
/// 与批量版本分开：曾经两者共用一个 handler，路径 id 被忽略，
/// 于是不带 body 的 `DELETE /api/accounts/{id}` 直接 415。
pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Value> {
    let removed = state
        .pool
        .repo()
        .delete_many(&[id])
        .await
        .map_err(internal)?;
    if removed == 0 {
        return Err(ApiError::admin(AppError::not_found_coded(
            format!("账号不存在: {id}"),
            "account_not_found",
        )));
    }
    let _ = state.pool.reload().await;
    Ok(Json(serde_json::json!({ "removed": removed, "id": id })))
}

// 批量维护

/// 批量操作请求。
#[derive(Debug, Deserialize, Default)]
pub struct BatchRequest {
    /// 目标账号；为空表示「全部需要处理的账号」。
    #[serde(default)]
    pub ids: Vec<Uuid>,
}

/// 批量响应。
#[derive(Debug, Serialize)]
pub struct BatchResponse {
    /// 操作 ID，用于轮询进度。
    pub operation_id: Uuid,
    /// 目标数量。
    pub total: usize,
}

/// `POST /api/accounts/sync`
pub async fn sync_accounts(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BatchRequest>,
) -> ApiResult<BatchResponse> {
    let ids = resolve_targets(&state, body.ids, TargetKind::Sync).await?;
    let deadline = Deadline::in_secs(900.0);
    let operation_id = gpt2apirs_account::spawn_sync_batch(
        state.pool.clone(),
        state.progress.clone(),
        ids.clone(),
        deadline,
        4,
    );
    Ok(Json(BatchResponse {
        operation_id,
        total: ids.len(),
    }))
}

/// `POST /api/accounts/refresh-token`
pub async fn refresh_tokens(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BatchRequest>,
) -> ApiResult<BatchResponse> {
    let ids = resolve_targets(&state, body.ids, TargetKind::Refresh).await?;
    let deadline = Deadline::in_secs(600.0);
    let operation_id = gpt2apirs_account::spawn_refresh_batch(
        state.pool.clone(),
        state.progress.clone(),
        ids.clone(),
        deadline,
        4,
    );
    Ok(Json(BatchResponse {
        operation_id,
        total: ids.len(),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Sync,
    Refresh,
}

async fn resolve_targets(
    state: &AppState,
    ids: Vec<Uuid>,
    kind: TargetKind,
) -> Result<Vec<Uuid>, ApiError> {
    if !ids.is_empty() {
        return Ok(ids);
    }
    let accounts = state.pool.repo().list_all().await.map_err(internal)?;
    let now = Utc::now();
    Ok(accounts
        .into_iter()
        .filter(|account| match kind {
            TargetKind::Sync => gpt2apirs_account::sync::needs_sync(account, now),
            TargetKind::Refresh => gpt2apirs_account::sync::needs_token_refresh(account, now),
        })
        .map(|account| account.id)
        .collect())
}

/// `GET /api/accounts/operations/{id}`
pub async fn get_operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<OperationProgress> {
    state
        .progress
        .get(id)
        .map(Json)
        .ok_or_else(|| {
            ApiError::admin(AppError::not_found(format!("操作 {id} 不存在或已过期")))
        })
}

// 账号测试

/// 测试请求。
#[derive(Debug, Deserialize, Default)]
pub struct TestRequest {
    /// 提示词。
    pub prompt: Option<String>,
    /// 模型。
    pub model: Option<String>,
}

/// 测试响应。
#[derive(Debug, Serialize)]
pub struct TestResponse {
    /// `success` / `failed`。
    pub status: &'static str,
    /// 耗时毫秒。
    pub elapsed_ms: i64,
    /// 图片字节数（成功时）。
    pub bytes: Option<usize>,
    /// 图片尺寸。
    pub size: Option<String>,
    /// 错误信息。
    pub message: Option<String>,
}

const DEFAULT_TEST_PROMPT: &str = "一只慵懒的猫咪蜷在柔软毛毯上睡觉，午后暖光，温馨室内，细节丰富。";

/// `POST /api/accounts/{id}/test`
///
/// **真实生成一张图** —— 同步只能证明 token 有效，测试能证明配额和链路都通。
pub async fn test_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    body: Option<Json<TestRequest>>,
) -> ApiResult<TestResponse> {
    let body = body.map(|Json(value)| value).unwrap_or_default();
    let prompt = body
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_TEST_PROMPT)
        .to_string();
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| gpt2apirs_generate::models::default_image_model().to_string());
    if !gpt2apirs_generate::models::is_supported_image(&model) {
        return Err(ApiError::admin(AppError::invalid_param(
            format!("不支持的模型 {model:?}"),
            "model",
        )));
    }

    let deadline = default_request_deadline(&state.settings);
    let filter = SelectionFilter {
        purpose: Purpose::Image,
        only: Some(id),
        ..Default::default()
    };
    let lease = state.pool.acquire(&filter, deadline).await.map_err(|error| match error {
        PoolError::NoCandidate { .. } => ApiError::admin(AppError::ServiceUnavailable {
            message: "该账号当前不可用（状态非正常或配额为 0）".to_string(),
            code: "account_not_available".to_string(),
        }),
        other => pool_error(other),
    })?;

    let mut image_request = ImageRequest::new(prompt).with_n(1);
    image_request.model = model.clone();
    image_request.upstream_model = gpt2apirs_generate::models::upstream_model_for(&model).to_string();

    let started = std::time::Instant::now();
    let client = state.client_for(&lease.account().proxy).map_err(internal)?;
    let outcome = generate_images(&client, lease.access_token(), &image_request, deadline).await;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    // 空图视为失败：见 v1_images 的同名注释
    let success = outcome
        .as_ref()
        .map(|(result, _)| !result.images.is_empty())
        .unwrap_or(false);
    // 测试同样计入成功/失败与配额消耗
    let _ = state.pool.mark_result(lease.id(), success).await;
    drop(lease);

    match outcome {
        Ok((result, _)) => {
            let first = result.images.first();
            Ok(Json(TestResponse {
                status: "success",
                elapsed_ms,
                bytes: first.map(|image| image.size),
                size: first.map(|image| format!("{}x{}", image.width, image.height)),
                message: None,
            }))
        }
        Err(error) => Ok(Json(TestResponse {
            status: "failed",
            elapsed_ms,
            bytes: None,
            size: None,
            message: Some(error.to_string()),
        })),
    }
}

// OAuth

/// 起始请求。
#[derive(Debug, Deserialize, Default)]
pub struct OAuthStartRequest {
    /// 邮箱提示。
    pub email_hint: Option<String>,
}

/// `POST /api/accounts/oauth/start`
pub async fn oauth_start(
    State(state): State<Arc<AppState>>,
    body: Option<Json<OAuthStartRequest>>,
) -> ApiResult<Value> {
    let body = body.map(|Json(value)| value).unwrap_or_default();
    let start = gpt2apirs_account::oauth::start(&state.oauth, body.email_hint.as_deref());
    Ok(Json(serde_json::to_value(start).unwrap_or(Value::Null)))
}

/// 完成请求。
#[derive(Debug, Deserialize)]
pub struct OAuthFinishRequest {
    /// 会话 ID。
    pub session_id: Uuid,
    /// 回调 URL 或裸 code。
    pub callback: String,
}

/// `POST /api/accounts/oauth/finish`
pub async fn oauth_finish(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OAuthFinishRequest>,
) -> ApiResult<ImportResponse> {
    let deadline = Deadline::in_secs(60.0);
    // OAuth 回调要访问上游，同样走统一出口。
    let client = state.client_for("").map_err(internal)?;
    let new_account =
        gpt2apirs_account::oauth::finish(&client, &state.oauth, body.session_id, &body.callback, deadline)
            .await
            .map_err(|error| {
                ApiError::admin(AppError::invalid_param(
                    error.to_string(),
                    "callback",
                ))
            })?;

    let label = new_account
        .email
        .clone()
        .unwrap_or_else(|| "oauth-account".to_string());
    let report = state
        .pool
        .repo()
        .upsert_many(&[new_account])
        .await
        .map_err(internal)?;
    let _ = state.pool.reload().await;

    let (outcome, reason) = if !report.inserted.is_empty() {
        ("added", None)
    } else if !report.merged.is_empty() {
        ("merged", Some("已存在，合并了新提供的字段".to_string()))
    } else {
        ("skipped", Some("access_token 已存在且无新字段".to_string()))
    };

    // OAuth 只涉及一个账号，顺手同步一次配额 —— 否则用户还得再点一次
    // 「同步配额」才知道这个号能不能用。
    let target_ids: Vec<Uuid> = report
        .inserted
        .iter()
        .chain(report.merged.iter())
        .map(|account| account.id)
        .collect();
    let sync_operation_id = if target_ids.is_empty() {
        None
    } else {
        Some(gpt2apirs_account::spawn_sync_batch(
            state.pool.clone(),
            state.progress.clone(),
            target_ids,
            Deadline::in_secs(120.0),
            1,
        ))
    };

    Ok(Json(ImportResponse {
        added: report.added(),
        merged: report.merged_count(),
        skipped: report.skipped_blank,
        items: vec![ImportItem {
            label,
            outcome,
            reason,
        }],
        sync_operation_id,
    }))
}

// 导出

/// 导出请求。
#[derive(Debug, Deserialize, Default)]
pub struct ExportRequest {
    /// 要导出的账号；为空表示全部。
    #[serde(default)]
    pub ids: Vec<Uuid>,
    /// `json`（默认）或 `txt`。
    pub format: Option<String>,
}

/// `POST /api/accounts/export`
pub async fn export_accounts(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ExportRequest>,
) -> ApiResult<Value> {
    let accounts = if body.ids.is_empty() {
        state.pool.repo().list_all().await.map_err(internal)?
    } else {
        let mut collected = Vec::new();
        for id in &body.ids {
            if let Some(account) = state.pool.repo().get_by_id(*id).await.map_err(internal)? {
                collected.push(account);
            }
        }
        collected
    };

    let format = body.format.as_deref().unwrap_or("json");
    match format {
        "txt" => Ok(Json(serde_json::json!({
            "format": "txt",
            "content": build_token_txt(&accounts),
        }))),
        "json" => {
            let report = build_export(&accounts);
            Ok(Json(serde_json::json!({
                "format": "json",
                "items": report.items,
                "skipped": report.skipped,
            })))
        }
        other => Err(ApiError::admin(AppError::invalid_param(
            format!("未知导出格式 {other:?}，可选 json/txt"),
            "format",
        ))),
    }
}

/// 让 `OperationKind` 在外部可见（供文档/测试引用）。
pub const _KINDS: &[OperationKind] = &[
    OperationKind::Sync,
    OperationKind::RefreshToken,
    OperationKind::Test,
    OperationKind::Import,
];

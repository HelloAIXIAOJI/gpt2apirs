//! 生图的「换号重试 + 并行」执行层。
//!
//! 三个决定：能否换号看 [`FailureKind::switch_account`]（内容策略与验证码换号没用）；
//! 换几次受 `image_max_account_attempts` 限制（默认 4，防止烧穿整池）；
//! `n > 1` 时每张图一个独立任务 —— 单账号串行 n 次不是并行。

use std::collections::HashSet;
use std::sync::Arc;

use gpt2apirs_account::{PoolError, SelectionFilter};
use gpt2apirs_core::{AppError, Deadline};
use gpt2apirs_generate::error::{FailureKind, GenerateError};
use gpt2apirs_generate::upload::UploadedFile;
use gpt2apirs_generate::{FlowDiagnostics, ImageRequest, PollPolicy, generate_images};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// 一次成功的生成。
pub struct SlotOutcome {
    /// 图片（至少 1 张）。
    pub images: Vec<gpt2apirs_generate::GeneratedImage>,
    /// 服务的账号。
    pub account_id: Uuid,
    /// 诊断。
    pub diag: FlowDiagnostics,
}

/// 槽位失败的原因。
///
/// **必须区分「池里没号」和「上游失败」**：
/// 前者对外是 `503 no_available_account`（运维该去导入账号），
/// 后者是上游/业务问题。混成一种会让运维照着错误的提示去排查。
enum SlotFailure {
    /// 池子层面的问题。
    Pool(PoolError),
    /// 生成层面的问题（已分类）。
    Generate(GenerateError),
}

/// 单个槽位的执行结果（可能失败）。
struct SlotResult {
    outcome: Option<SlotOutcome>,
    error: Option<SlotFailure>,
}

fn attempts_limit(state: &AppState) -> usize {
    state
        .settings
        .get_i64("image_max_account_attempts")
        .max(1) as usize
}

fn acquire_error(error: PoolError) -> ApiError {
    match error {
        PoolError::NoCandidate { .. } => ApiError::openai(AppError::ServiceUnavailable {
            message: "无可用账号，请先通过 POST /api/accounts 导入".to_string(),
            code: "no_available_account".to_string(),
        }),
        PoolError::DeadlineExceeded => ApiError::openai(AppError::ServiceUnavailable {
            message: "等待账号槽位超时，请稍后重试".to_string(),
            code: "account_pool_timeout".to_string(),
        }),
        other => ApiError::openai(AppError::Internal(anyhow::anyhow!("{other}"))),
    }
}

/// 在池子里跑一个操作，失败时按分类换号重试。
///
/// `op` 收到账号 token，返回成功值或分类错误。
async fn with_retry<F, Fut>(state: &Arc<AppState>, deadline: Deadline, op: F) -> SlotResult
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<SlotOutcome, GenerateError>>,
{
    let max_attempts = attempts_limit(state);
    let mut excluded: HashSet<Uuid> = HashSet::new();
    let mut last_error: Option<SlotFailure> = None;

    for _ in 0..max_attempts {
        let filter = SelectionFilter {
            exclude: excluded.clone(),
            ..SelectionFilter::for_image()
        };
        let lease = match state.pool.acquire(&filter, deadline).await {
            Ok(lease) => lease,
            Err(PoolError::NoCandidate { .. }) if !excluded.is_empty() => {
                // 排除掉失败账号后没号了：把上一次的失败原因带出去
                break;
            }
            Err(error) => {
                return SlotResult {
                    outcome: None,
                    error: Some(SlotFailure::Pool(error)),
                };
            }
        };
        let account_id = lease.id();
        crate::call_log::note_account(account_id);
        let token = lease.access_token().to_string();
        let proxy = lease.account().proxy.trim().to_string();

        let result = op(token, proxy).await;
        match result {
            Ok(mut outcome) => {
                outcome.account_id = account_id;
                state.pool.mark_result(account_id, true).await.ok();
                return SlotResult {
                    outcome: Some(outcome),
                    error: None,
                };
            }
            Err(error) => {
                // 空图/内容策略不该扣成功计数，但失败计数要记
                state.pool.mark_result(account_id, false).await.ok();
                if error.kind.blames_account() {
                    // 账号侧问题：走失效判定（内含保护期）
                    state.pool.handle_invalid(account_id, &error.message).await.ok();
                }
                let switch = error.kind.switch_account();
                last_error = Some(SlotFailure::Generate(error));
                if !switch {
                    // 内容策略 / 验证码 / 协议错误：换号没用，立刻停
                    break;
                }
                excluded.insert(account_id);
            }
        }
        drop(lease);
    }

    SlotResult {
        outcome: None,
        error: last_error,
    }
}

/// 把失败槽位转成对外错误。
pub(crate) fn slot_error(error: GenerateError) -> ApiError {
    let status = match error.kind {
        FailureKind::ContentPolicy => 400,
        FailureKind::TurnstileRequired | FailureKind::ArkoseRequired => 503,
        FailureKind::QuotaExhausted | FailureKind::RateLimited => 503,
        _ => 502,
    };
    ApiError::openai(AppError::Upstream {
        message: error.message,
        code: error.kind.code().to_string(),
        retryable: error.kind.retryable(),
        status,
    })
}

/// 文生图：`n` 张并行 + 换号重试。
pub async fn run_generation(
    state: &Arc<AppState>,
    mut request: ImageRequest,
    deadline: Deadline,
) -> Result<Vec<SlotOutcome>, ApiError> {
    // SSE 没带回图片时的兜底轮询，读三个 image_poll_* 设置。
    request.poll = PollPolicy::from_settings(&state.settings);
    let total = request.n.clamp(1, 4) as usize;
    let mut set: JoinSet<SlotResult> = JoinSet::new();

    for _ in 0..total {
        let outer = state.clone();
        let mut single = request.clone();
        single.n = 1; // 每个槽位只产 1 张，张数由任务数决定
        set.spawn(async move {
            let inner = outer.clone();
            with_retry(&outer, deadline, move |token, proxy| {
                let state = inner.clone();
                let single = single.clone();
                async move {
                    let client = state.client_for(&proxy).map_err(|error| {
                        GenerateError::new(FailureKind::Network, format!("代理客户端构建失败：{error}"))
                    })?;
                    let (result, diag) = generate_images(&client, &token, &single, deadline)
                        .await?;
                    if result.images.is_empty() {
                        return Err(GenerateError::new(
                            FailureKind::EmptyImages,
                            "上游没有返回图片",
                        ));
                    }
                    Ok(SlotOutcome {
                        images: result.images,
                        account_id: Uuid::nil(),
                        diag,
                    })
                }
            })
            .await
        });
    }

    collect(set, total).await
}

/// 生成前的磁盘空间预检。
///
/// 空间不足时直接拒绝：否则上游已经出图、配额已经扣掉，本地却存不下。
/// 读不到剩余空间不算错误（按放行处理），只记一条 WARN。
pub fn ensure_disk_space() -> Result<(), ApiError> {
    const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;
    let root = match gpt2apirs_core::paths::data_dir() {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "定位数据目录失败，跳过磁盘预检");
            return Ok(());
        }
    };
    match gpt2apirs_core::disk::available_bytes(&root) {
        Ok(free) if free >= MIN_FREE_BYTES => Ok(()),
        Ok(free) => Err(ApiError::openai(AppError::ServiceUnavailable {
            message: format!(
                "磁盘剩余空间不足（{} MB），拒绝生成以免丢图",
                free / (1024 * 1024)
            ),
            code: "disk_full".to_string(),
        })),
        Err(error) => {
            tracing::warn!(%error, "读取磁盘剩余空间失败，按放行处理");
            Ok(())
        }
    }
}

/// 把生成结果写进 `image_assets`，供后台按账号/时间查询。
///
/// 失败只记日志：元数据写不进去不该让用户拿不到图。
pub async fn record_assets(state: &Arc<AppState>, slots: &[SlotOutcome], prompt: &str) {
    let repo = gpt2apirs_storage::repo::ImageAssetRepo::new(state.db.clone());
    for slot in slots {
        for image in &slot.images {
            if image.rel_path.is_empty() {
                continue;
            }
            let asset = gpt2apirs_storage::repo::NewImageAsset {
                rel_path: image.rel_path.clone(),
                mime: image.mime.to_string(),
                bytes: image.bytes.len() as i64,
                width: image.width as i32,
                height: image.height as i32,
                account_id: if slot.account_id.is_nil() { None } else { Some(slot.account_id) },
                prompt: Some(prompt.to_string()),
            };
            if let Err(error) = repo.insert(&asset).await {
                tracing::warn!(%error, path = %image.rel_path, "写图片元数据失败");
            }
        }
    }
}

/// 图生图：上传参考图后并行 + 换号重试。
///
/// 参考图的上传与账号绑定，所以**每次换号都要重新上传**。
pub async fn run_edit(
    state: &Arc<AppState>,
    mut request: ImageRequest,
    prepared: Vec<(Vec<u8>, String)>,
    deadline: Deadline,
) -> Result<Vec<SlotOutcome>, ApiError> {
    request.poll = PollPolicy::from_settings(&state.settings);
    let total = request.n.clamp(1, 4) as usize;
    let mut set: JoinSet<SlotResult> = JoinSet::new();

    for _ in 0..total {
        let outer = state.clone();
        let prepared = prepared.clone();
        let mut single = request.clone();
        single.n = 1;
        set.spawn(async move {
            let inner = outer.clone();
            with_retry(&outer, deadline, move |token, proxy| {
                let state = inner.clone();
                let single = single.clone();
                let prepared = prepared.clone();
                async move {
                    let client = state.client_for(&proxy).map_err(|error| {
                        GenerateError::new(FailureKind::Network, format!("代理客户端构建失败：{error}"))
                    })?;
                    let mut references: Vec<gpt2apirs_protocol::conversation::ImageReference> =
                        Vec::with_capacity(prepared.len());
                    for (bytes, filename) in &prepared {
                        let uploaded: UploadedFile = gpt2apirs_generate::upload::upload_image(
                            &client,
                            &token,
                            gpt2apirs_http::DEFAULT_USER_AGENT,
                            bytes,
                            Some(filename),
                            deadline,
                        )
                        .await
                        .map_err(|error| {
                            GenerateError::new(
                                FailureKind::Network,
                                format!("上传参考图失败：{error}"),
                            )
                        })?;
                        references.push(uploaded.as_reference());
                    }
                    let single = single.with_images(references);
                    let (result, diag) = generate_images(&client, &token, &single, deadline)
                        .await?;
                    if result.images.is_empty() {
                        return Err(GenerateError::new(
                            FailureKind::EmptyImages,
                            "上游没有返回图片",
                        ));
                    }
                    Ok(SlotOutcome {
                        images: result.images,
                        account_id: Uuid::nil(),
                        diag,
                    })
                }
            })
            .await
        });
    }

    collect(set, total).await
}

/// 汇总并行结果。
async fn collect(mut set: JoinSet<SlotResult>, total: usize) -> Result<Vec<SlotOutcome>, ApiError> {
    let mut successes = Vec::new();
    let mut first_error: Option<SlotFailure> = None;
    let mut failed = 0usize;

    while let Some(joined) = set.join_next().await {
        let slot = match joined {
            Ok(slot) => slot,
            Err(error) => {
                failed += 1;
                first_error.get_or_insert_with(|| {
                    SlotFailure::Generate(GenerateError::new(
                        FailureKind::Protocol,
                        format!("任务异常退出：{error}"),
                    ))
                });
                continue;
            }
        };
        match slot.outcome {
            Some(outcome) => successes.push(outcome),
            None => {
                failed += 1;
                if let Some(error) = slot.error {
                    // 池子问题优先级最高；其次内容策略这类「换号也没用」的错误（信息量最大）
                    let replace = match (&first_error, &error) {
                        (None, _) => true,
                        (Some(SlotFailure::Generate(_)), SlotFailure::Pool(_)) => true,
                        (
                            Some(SlotFailure::Generate(current)),
                            SlotFailure::Generate(next),
                        ) => {
                            current.kind == FailureKind::EmptyImages
                                && next.kind != FailureKind::EmptyImages
                        }
                        _ => false,
                    };
                    if replace {
                        first_error = Some(error);
                    }
                }
            }
        }
    }

    if !successes.is_empty() {
        if failed > 0 {
            tracing::warn!(
                ok = successes.len(),
                failed,
                total,
                "部分槽位失败，返回已生成的图片"
            );
        }
        return Ok(successes);
    }

    // 全失败：**池子问题优先**。池空时说「上游异常」会把运维引到错方向。
    Err(match first_error {
        Some(SlotFailure::Pool(error)) => acquire_error(error),
        Some(SlotFailure::Generate(error)) => slot_error(error),
        None => ApiError::openai(AppError::Internal(anyhow::anyhow!(
            "所有槽位都失败且没有错误信息"
        ))),
    })
}

//! 账号仓储。
//!
//! 设计见 `docs/account-pool-design.md` 第三章与 D3/D9/D10。
//!
//! # 只支持 PostgreSQL
//! ADR-008 修订后放弃 SQLite，因此这里没有后端分支。
//!
//! # 并发
//! 逐行 UPDATE 依赖 PG 行锁；只有「批量导入去重」和「选号游标」需要
//! `pg_advisory_xact_lock`（见 [`AccountRepo::insert_many`]）。

use chrono::{DateTime, Utc};
use gpt2apirs_storage::pool::StorageError;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::model::{Account, AccountStatus, Plan, Source};

/// 账号层错误。
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// 存储错误。
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// 数据库错误。
    #[error("账号库操作失败：{0}")]
    Db(#[from] sqlx::Error),

    /// 账号不存在。
    #[error("账号不存在：{0}")]
    NotFound(Uuid),
}

/// 新建账号的输入。
#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    /// access token（必填）。
    pub access_token: String,
    /// refresh token。
    pub refresh_token: Option<String>,
    /// id token。
    pub id_token: Option<String>,
    /// 邮箱。
    pub email: Option<String>,
    /// 上游 account_id。
    pub upstream_account_id: Option<String>,
    /// 套餐。
    pub plan: Plan,
    /// 来源。
    pub source: Source,
    /// 初始配额。
    pub quota: i32,
    /// 配额是否已确认。
    pub quota_confirmed: bool,
    /// 要导入到哪个桶；`None` = 默认桶。
    pub bucket_id: Option<Uuid>,
}

/// 部分更新。
#[derive(Debug, Clone, Default)]
pub struct AccountPatch {
    /// 状态。
    pub status: Option<AccountStatus>,
    /// 套餐。
    pub plan: Option<Plan>,
    /// 邮箱。
    pub email: Option<String>,
    /// 换桶（账号的出口跟着桶走，所以这也是「换出口」）。
    pub bucket_id: Option<Uuid>,
    /// 配额。
    pub quota: Option<i32>,
    /// 配额是否已确认。
    pub quota_confirmed: Option<bool>,
    /// 上游是否没有图片限额。
    pub quota_unlimited: Option<bool>,
    /// 限流恢复时间。
    pub restore_at: Option<Option<DateTime<Utc>>>,
    /// 最近错误（已脱敏）。
    pub last_error: Option<Option<String>>,
}

/// 列表筛选条件。
#[derive(Debug, Clone, Default)]
pub struct AccountFilter {
    /// 状态。
    pub status: Option<AccountStatus>,
    /// 套餐。
    pub plan: Option<Plan>,
    /// 来源。
    pub source: Option<Source>,
    /// 关键词（匹配 email / 上游 account_id / token sha 前缀）。
    pub keyword: Option<String>,
    /// 只看某个桶；`None` = 全部桶（「全部」那个入口）。
    pub bucket: Option<Uuid>,
    /// 分页：偏移。
    pub offset: i64,
    /// 分页：数量（0 表示不限）。
    pub limit: i64,
}

/// 账号仓储。
#[derive(Debug, Clone)]
pub struct AccountRepo {
    pool: PgPool,
}

impl AccountRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池引用（给需要事务的调用方）。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 写入一个账号。识别「是不是同一个账号」按三级顺序：
    ///
    /// 1. **同一 access_token**（sha256 相同） 合并非空字段，token 不动
    /// 2. **同一上游 account_id 或同一邮箱**  这是同一账号换了 token（典型：OAuth
    ///    拿回的新 AT 与旧的不是同一串），**轮换本行 token** 并合并字段
    /// 3. 都不匹配  插入新行
    ///
    /// 第 2 条是必须的：只按 token 判重会把「同一个号重新授权」变成两行，
    /// 结果是同一账号被当成两个并发槽位，配额也会被重复计算。
    pub async fn upsert(&self, input: &NewAccount) -> Result<UpsertOutcome, AccountError> {
        let mut tx = self.pool.begin().await?;
        acquire_pool_lock(&mut tx).await?;
        let outcome = self.upsert_inner(input, &mut tx).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// 批量 upsert（去重 + advisory lock）。
    pub async fn upsert_many(
        &self,
        inputs: &[NewAccount],
    ) -> Result<UpsertReport, AccountError> {
        let mut tx = self.pool.begin().await?;
        // 池级 advisory lock：避免并发导入同一批 token
        acquire_pool_lock(&mut tx).await?;

        let mut report = UpsertReport::default();
        for input in inputs {
            // 空白 token 不是账号，既不插入也不算「重复跳过」。
            // 调用方（导入层）应在进来之前过滤掉空行。
            if input.access_token.trim().is_empty() {
                report.skipped_blank += 1;
                continue;
            }
            match self.upsert_inner(input, &mut tx).await? {
                UpsertOutcome::Inserted(found) => report.inserted.push(found),
                UpsertOutcome::Merged(found) => report.merged.push(found),
            }
        }
        tx.commit().await?;
        Ok(report)
    }

    async fn upsert_inner(
        &self,
        input: &NewAccount,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<UpsertOutcome, AccountError> {
        let access_token = input.access_token.trim();
        let sha = Account::token_sha(access_token);

        // 身份优先取 JWT 自带的声明（权威），其次才用导入文件里写的字段。
        let identity_email = crate::credentials::token_email(access_token)
            .or_else(|| input.email.clone())
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let identity_account = crate::credentials::token_upstream_account_id(access_token)
            .or_else(|| input.upstream_account_id.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        // ---- 1) 同一 token ----
        let same_token: Option<Uuid> =
            sqlx::query_scalar("select id from accounts where access_token_sha = $1 for update")
                .bind(&sha)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some(id) = same_token {
            let account = merge_row(tx, id, input, access_token, &sha, false).await?;
            return Ok(UpsertOutcome::Merged(account));
        }

        // ---- 2) 同一账号换了 token ----
        let same_identity: Option<Uuid> = if let Some(account_id) = identity_account.as_deref() {
            sqlx::query_scalar(
                "select id from accounts where upstream_account_id = $1 \
                 order by created_at asc limit 1 for update",
            )
            .bind(account_id)
            .fetch_optional(&mut **tx)
            .await?
        } else {
            None
        };
        let same_identity = match same_identity {
            Some(id) => Some(id),
            None => match identity_email.as_deref() {
                Some(email) => {
                    sqlx::query_scalar(
                        "select id from accounts \
                         where lower(coalesce(email, '')) = $1 \
                         order by created_at asc limit 1 for update",
                    )
                    .bind(email)
                    .fetch_optional(&mut **tx)
                    .await?
                }
                None => None,
            },
        };
        if let Some(id) = same_identity {
            // 历史上可能已经存在同身份的多行（例如 OAuth 换回新 token 时插了第二行）。
            // 这里把它们并进保留行：计数相加、时间取极值，然后删掉多余的。
            absorb_duplicates(
                tx,
                id,
                identity_email.as_deref(),
                identity_account.as_deref(),
            )
            .await?;
            let account = merge_row(tx, id, input, access_token, &sha, true).await?;
            return Ok(UpsertOutcome::Merged(account));
        }

        // ---- 3) 全新 ----
        let new_id = Uuid::new_v4();
        sqlx::query(&format!(
            // 没指定桶就进默认桶：导入是主路径，不能因为漏传参数就报错。
            // 不能用 returning {COLUMNS}：那里面的 proxy 是桶的子查询，
            // INSERT 的 RETURNING 里引用目标表子查询不稳，改用「插完再查」。
            "insert into accounts ( \
                 id, access_token, access_token_sha, refresh_token, id_token, \
                 email, upstream_account_id, plan, source, quota, quota_confirmed, bucket_id \
             ) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11, \
                 coalesce($12, (select id from buckets where is_default limit 1))) \
             returning id",
        ))
        .bind(new_id)
        .bind(access_token)
        .bind(&sha)
        .bind(input.refresh_token.as_deref())
        .bind(input.id_token.as_deref())
        .bind(input.email.as_deref())
        .bind(input.upstream_account_id.as_deref())
        .bind(input.plan.as_str())
        .bind(input.source.as_str())
        .bind(input.quota)
        .bind(input.quota_confirmed)
        .bind(input.bucket_id)
        .fetch_one(&mut **tx)
        .await?;
        let row = sqlx::query(&format!("select {COLUMNS} from accounts where id = $1"))
            .bind(new_id)
            .fetch_one(&mut **tx)
            .await?;
        Ok(UpsertOutcome::Inserted(row_to_account(&row)))
    }

    /// 按 ID 查。
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Account>, AccountError> {
        let row = sqlx::query(&format!("select {COLUMNS} from accounts where id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| row_to_account(&r)))
    }

    /// 按 token 查（内部走 sha）。
    pub async fn get_by_token(&self, access_token: &str) -> Result<Option<Account>, AccountError> {
        let sha = Account::token_sha(access_token);
        let row = sqlx::query(&format!(
            "select {COLUMNS} from accounts where access_token_sha = $1"
        ))
        .bind(sha)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| row_to_account(&r)))
    }

    /// 全量（用于内存快照）。
    pub async fn list_all(&self) -> Result<Vec<Account>, AccountError> {
        let rows = sqlx::query(&format!("select {COLUMNS} from accounts order by created_at"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_account).collect())
    }

    /// 条件查询（分页）。
    pub async fn list(&self, filter: &AccountFilter) -> Result<Vec<Account>, AccountError> {
        let (where_sql, binds) = build_filter(filter);
        let limit = if filter.limit > 0 { filter.limit } else { DEFAULT_LIST_LIMIT };
        // where_sql 若存在占用了 $1..$n，limit/offset 必须顺延编号，否则类型冲突。
        let next = binds.len() + 1;
        let sql = format!(
            "select {COLUMNS} from accounts {where_sql} \
             order by created_at desc limit ${next} offset ${}",
            next + 1
        );
        let mut query = sqlx::query(&sql);
        for bind in binds {
            query = query.bind(bind);
        }
        let rows = query
            .bind(limit)
            .bind(filter.offset.max(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_account).collect())
    }

    /// 满足条件的总数。
    pub async fn count(&self, filter: &AccountFilter) -> Result<i64, AccountError> {
        let (where_sql, binds) = build_filter(filter);
        let sql = format!("select count(*) from accounts {where_sql}");
        let mut query = sqlx::query_scalar::<_, i64>(&sql);
        for bind in binds {
            query = query.bind(bind);
        }
        Ok(query.fetch_one(&self.pool).await?)
    }

    /// 部分更新。返回更新后的账号。
    pub async fn patch(&self, id: Uuid, patch: &AccountPatch) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set \
                 status          = coalesce($2, status), \
                 plan            = coalesce($3, plan), \
                 email           = coalesce($4, email), \
                 bucket_id       = coalesce($5, bucket_id), \
                 quota           = coalesce($6, quota), \
                 quota_confirmed = coalesce($7, quota_confirmed), \
                 quota_unlimited = coalesce($12, quota_unlimited), \
                 restore_at      = case when $8 then $9 else restore_at end, \
                 last_error      = case when $10 then $11 else last_error end \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .bind(patch.status.map(|s| s.as_str().to_string()))
        .bind(patch.plan.map(|p| p.as_str().to_string()))
        .bind(patch.email.clone())
        .bind(patch.bucket_id)
        .bind(patch.quota)
        .bind(patch.quota_confirmed)
        .bind(patch.restore_at.is_some())
        .bind(patch.restore_at.clone().flatten())
        .bind(patch.last_error.is_some())
        .bind(patch.last_error.clone().flatten())
        .bind(patch.quota_unlimited)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 记录一次使用结果。
    ///
    /// `success`：
    /// - 计数 +1、配额 -1；配额归零  `limited`
    /// - 若原为 `limited` 且配额恢复 > 0  `normal`
    ///
    /// `failure`：失败计数 +1，**状态不变**。
    pub async fn record_result(&self, id: Uuid, success: bool) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set \
                 success_count = success_count + case when $2 then 1 else 0 end, \
                 fail_count    = fail_count    + case when $2 then 0 else 1 end, \
                 quota         = case when $2 and not quota_unlimited then greatest(0, quota - 1) else quota end, \
                 status        = case \
                                     when $2 and not quota_unlimited and greatest(0, quota - 1) = 0 then 'limited' \
                                     when $2 and (quota_unlimited or quota > 0) and status = 'limited' then 'normal' \
                                     else status \
                                 end, \
                 last_used_at  = now() \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .bind(success)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 记录一次**文本**用途的使用结果。
    ///
    /// 与 [`record_result`] 的唯一差别：**不动 `quota`**。
    ///
    /// 文本对话不消耗图片配额，这是两套独立的额度。用 `record_result` 记文本
    /// 会让图片配额凭空减少，进而让调度以为账号快用完了。
    pub async fn record_text_result(
        &self,
        id: Uuid,
        success: bool,
    ) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set \
                 success_count = success_count + case when $2 then 1 else 0 end, \
                 fail_count    = fail_count    + case when $2 then 0 else 1 end, \
                 last_used_at  = now() \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .bind(success)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 记录一次「被判失效」。
    ///
    /// 只累加计数与时间，**不决定是否移除**（那是池的策略）。
    pub async fn record_invalid(
        &self,
        id: Uuid,
        error: &str,
    ) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set \
                 invalid_count   = invalid_count + 1, \
                 last_invalid_at = now(), \
                 last_error      = $2, \
                 last_error_at   = now() \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .bind(error)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 清除失效痕迹（刷新成功后调用）。
    pub async fn clear_invalid(&self, id: Uuid) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set \
                 invalid_count = 0, \
                 last_error    = null, \
                 last_error_at = null, \
                 status        = case when status = 'abnormal' then 'normal' else status end \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 替换 token（轮换 / 刷新），**同一行**更新。
    pub async fn replace_tokens(
        &self,
        id: Uuid,
        access_token: &str,
        refresh_token: Option<&str>,
        id_token: Option<&str>,
    ) -> Result<Account, AccountError> {
        let sha = Account::token_sha(access_token);
        let row = sqlx::query(&format!(
            "update accounts set \
                 access_token     = $2, \
                 access_token_sha = $3, \
                 refresh_token    = coalesce($4, refresh_token), \
                 id_token         = coalesce($5, id_token), \
                 last_refresh_at  = now() \
             where id = $1 \
             returning {COLUMNS}",
        ))
        .bind(id)
        .bind(access_token)
        .bind(sha)
        .bind(refresh_token)
        .bind(id_token)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 标记「刚做过远端校验」。
    ///
    /// 只更新时间戳；账号字段的合并由调用方（sync）通过 [`AccountRepo::patch`] 完成。
    pub async fn mark_checked(&self, id: Uuid) -> Result<Account, AccountError> {
        let row = sqlx::query(&format!(
            "update accounts set last_checked_at = now() where id = $1 returning {COLUMNS}",
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AccountError::NotFound(id))?;
        Ok(row_to_account(&row))
    }

    /// 批量删除。返回删除数量。
    pub async fn delete_many(&self, ids: &[Uuid]) -> Result<u64, AccountError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query("delete from accounts where id = any($1)")
            .bind(ids)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 统计：`(总数, 各状态计数, 总配额, 总成功, 总失败)`。
    pub async fn stats(&self) -> Result<AccountPoolStats, AccountError> {
        let row = sqlx::query(
            "select \
                 count(*)::bigint                                        as total, \
                 count(*) filter (where status = 'normal')::bigint        as normal, \
                 count(*) filter (where status = 'limited')::bigint       as limited, \
                 count(*) filter (where status = 'abnormal')::bigint      as abnormal, \
                 count(*) filter (where status = 'disabled')::bigint      as disabled, \
                 coalesce(sum(quota) filter (where status = 'normal'), 0)::bigint as total_quota, \
                 coalesce(sum(success_count), 0)::bigint                  as total_success, \
                 coalesce(sum(fail_count), 0)::bigint                     as total_fail \
             from accounts",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(AccountPoolStats {
            total: row.get("total"),
            normal: row.get("normal"),
            limited: row.get("limited"),
            abnormal: row.get("abnormal"),
            disabled: row.get("disabled"),
            total_quota: row.get("total_quota"),
            total_success: row.get("total_success"),
            total_fail: row.get("total_fail"),
        })
    }
}

/// 一次 upsert 的结果。
#[derive(Debug, Clone)]
pub enum UpsertOutcome {
    /// 全新插入。
    Inserted(Account),
    /// 已存在，合并了非空字段。
    Merged(Account),
}

impl UpsertOutcome {
    /// 账号引用。
    pub fn account(&self) -> &Account {
        match self {
            UpsertOutcome::Inserted(account) | UpsertOutcome::Merged(account) => account,
        }
    }

    /// 是否属于「新增」。
    pub fn is_inserted(&self) -> bool {
        matches!(self, UpsertOutcome::Inserted(_))
    }
}

/// 批量 upsert 的报告。
#[derive(Debug, Clone, Default)]
pub struct UpsertReport {
    /// 新增的账号。
    pub inserted: Vec<Account>,
    /// 合并了字段的已有账号。
    pub merged: Vec<Account>,
    /// 因 token 为空被忽略的条目数。
    pub skipped_blank: usize,
}

impl UpsertReport {
    /// 新增数。
    pub fn added(&self) -> usize {
        self.inserted.len()
    }

    /// 合并数。
    pub fn merged_count(&self) -> usize {
        self.merged.len()
    }
}

/// 账号池统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct AccountPoolStats {
    /// 总数。
    pub total: i64,
    /// 正常数。
    pub normal: i64,
    /// 限流数。
    pub limited: i64,
    /// 异常数。
    pub abnormal: i64,
    /// 禁用数。
    pub disabled: i64,
    /// 正常账号的配额总和。
    pub total_quota: i64,
    /// 累计成功。
    pub total_success: i64,
    /// 累计失败。
    pub total_fail: i64,
}

/// [`AccountFilter::limit`] 为 0 时使用的默认页大小。
///
/// 账号量级在千以内，一次取完不会有压力。
const DEFAULT_LIST_LIMIT: i64 = 1000;

/// 关键词达到这个长度才按「完整 token」做精确查找。
///
/// 原因：D10 把 token 的唯一索引换成了 sha256，**哈希不可逆**，
/// 因此无法按 token 片段搜索。输入够长时视为完整 token，算哈希精确匹配。
const MIN_TOKEN_LOOKUP_LEN: usize = 32;

/// 列清单（保持与 [`row_to_account`] 对齐）。
/// 列清单（保持与 [`row_to_account`] 对齐）。
///
/// `proxy` 已经不是账号自己的列了：它从所属桶的出口算出来。
/// 放在这里而不是调用点，是为了让「账号用哪个出口」只有一处定义。
const COLUMNS: &str = "\
    id, access_token, refresh_token, id_token, email, upstream_account_id, \
    plan, source, status, quota, quota_confirmed, quota_unlimited, restore_at, bucket_id, \
    coalesce((select b.proxy_url from buckets b where b.id = accounts.bucket_id), '') as proxy, \
    success_count, fail_count, invalid_count, \
    last_used_at, last_invalid_at, last_error, last_error_at, \
    last_checked_at, last_refresh_at, created_at, updated_at";

fn row_to_account(row: &PgRow) -> Account {
    Account {
        id: row.get("id"),
        access_token: row.get("access_token"),
        refresh_token: row.get("refresh_token"),
        id_token: row.get("id_token"),
        email: row.get("email"),
        upstream_account_id: row.get("upstream_account_id"),
        plan: Plan::parse(row.get::<String, _>("plan").as_str()),
        source: Source::parse(row.get::<String, _>("source").as_str()).unwrap_or_else(|| {
            tracing::warn!(value = %row.get::<String, _>("source"), "账号来源是未知值，按 web 处理");
            Source::Web
        }),
        status: AccountStatus::parse(row.get::<String, _>("status").as_str()).unwrap_or_else(|| {
            tracing::warn!(value = %row.get::<String, _>("status"), "账号状态是未知值，按异常处理（不会被选中）");
            AccountStatus::Abnormal
        }),
        quota: row.get("quota"),
        quota_confirmed: row.get("quota_confirmed"),
        quota_unlimited: row.get("quota_unlimited"),
        restore_at: row.get("restore_at"),
        bucket_id: row.get("bucket_id"),
        proxy: row.get("proxy"),
        success_count: row.get("success_count"),
        fail_count: row.get("fail_count"),
        invalid_count: row.get("invalid_count"),
        last_used_at: row.get("last_used_at"),
        last_invalid_at: row.get("last_invalid_at"),
        last_error: row.get("last_error"),
        last_error_at: row.get("last_error_at"),
        last_checked_at: row.get("last_checked_at"),
        last_refresh_at: row.get("last_refresh_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

async fn acquire_pool_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), AccountError> {
    sqlx::query("select pg_advisory_xact_lock(hashtext('account_pool'))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 把同身份的其它行并入保留行，然后删除它们。
///
/// **只合并统计量，不合并配额**：同一个账号的多行看到的是同一份真实配额，
/// 相加会得到双倍假额度，进而让调度误判。
///
/// 合并规则：计数相加（历史不该因合并丢失）；`last_used_at` 取晚、`created_at` 取早；
/// `quota` / `status` / `invalid_count` 保留原值 —— 配额相加、连续失效次数相加都没有意义。
async fn absorb_duplicates(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    keep_id: Uuid,
    email: Option<&str>,
    upstream_account_id: Option<&str>,
) -> Result<u64, AccountError> {
    if email.is_none() && upstream_account_id.is_none() {
        return Ok(0);
    }
    // 与保留行同身份、但不是保留行本身
    let dup_filter = "id <> $1
           and ( ($2::text is not null and lower(coalesce(email, '')) = $2)
              or ($3::text is not null and upstream_account_id = $3) )";

    let summary = sqlx::query(&format!(
        "select coalesce(sum(success_count), 0)::bigint as total_success, \
                coalesce(sum(fail_count), 0)::bigint    as total_fail, \
                max(last_used_at)                       as last_used_at, \
                min(created_at)                         as created_at, \
                count(*)::bigint                        as rows \
         from accounts where {dup_filter}",
    ))
    .bind(keep_id)
    .bind(email)
    .bind(upstream_account_id)
    .fetch_one(&mut **tx)
    .await?;

    let duplicate_rows: i64 = summary.get("rows");
    if duplicate_rows == 0 {
        return Ok(0);
    }
    let total_success: i64 = summary.get("total_success");
    let total_fail: i64 = summary.get("total_fail");
    let last_used_at: Option<chrono::DateTime<chrono::Utc>> = summary.get("last_used_at");
    let created_at: Option<chrono::DateTime<chrono::Utc>> = summary.get("created_at");

    sqlx::query(
        "update accounts set \
             success_count = success_count + $2, \
             fail_count    = fail_count + $3, \
             last_used_at  = case \
                                 when $4::timestamptz is null then last_used_at \
                                 when last_used_at is null then $4 \
                                 else greatest(last_used_at, $4) \
                             end, \
             created_at    = case \
                                 when $5::timestamptz is null then created_at \
                                 else least(created_at, $5) \
                             end \
         where id = $1",
    )
    .bind(keep_id)
    .bind(total_success)
    .bind(total_fail)
    .bind(last_used_at)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;

    let removed = sqlx::query(&format!("delete from accounts where {dup_filter}"))
        .bind(keep_id)
        .bind(email)
        .bind(upstream_account_id)
        .execute(&mut **tx)
        .await?;

    tracing::info!(
        keep = %keep_id,
        removed = removed.rows_affected(),
        merged_success = total_success,
        merged_fail = total_fail,
        "合并同身份重复账号"
    );
    Ok(removed.rows_affected())
}

/// 合并到指定行。`rotate_token` 为真时同时把本行的 access_token 换成新的。
async fn merge_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    input: &NewAccount,
    access_token: &str,
    sha: &str,
    rotate_token: bool,
) -> Result<Account, AccountError> {
    let row = sqlx::query(&format!(
        "update accounts set \
             access_token        = case when $2 then $3 else access_token end, \
             access_token_sha    = case when $2 then $4 else access_token_sha end, \
             refresh_token       = coalesce($5, refresh_token), \
             id_token            = coalesce($6, id_token), \
             email               = coalesce($7, email), \
             upstream_account_id = coalesce($8, upstream_account_id), \
             plan                = case when $9 = 'unknown' then plan else $9 end, \
             source              = $10, \
             bucket_id           = coalesce($11, bucket_id) \
         where id = $1 \
         returning {COLUMNS}",
    ))
    .bind(id)
    .bind(rotate_token)
    .bind(access_token)
    .bind(sha)
    .bind(input.refresh_token.as_deref())
    .bind(input.id_token.as_deref())
    .bind(input.email.as_deref())
    .bind(input.upstream_account_id.as_deref())
    .bind(input.plan.as_str())
    .bind(input.source.as_str())
    .bind(input.bucket_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row_to_account(&row))
}

fn build_filter(filter: &AccountFilter) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(status) = filter.status {
        binds.push(status.as_str().to_string());
        clauses.push(format!("status = ${}", binds.len()));
    }
    if let Some(plan) = filter.plan {
        binds.push(plan.as_str().to_string());
        clauses.push(format!("plan = ${}", binds.len()));
    }
    if let Some(source) = filter.source {
        binds.push(source.as_str().to_string());
        clauses.push(format!("source = ${}", binds.len()));
    }
    if let Some(bucket) = filter.bucket {
        // binds 是字符串表，uuid 转文本比较即可（bucket_id 上有索引，够快）
        binds.push(bucket.to_string());
        clauses.push(format!("bucket_id::text = ${}", binds.len()));
    }
    if let Some(keyword) = filter.keyword.as_deref() {
        let trimmed = keyword.trim();
        if !trimmed.is_empty() {
            binds.push(format!("%{}%", trimmed.to_ascii_lowercase()));
            let n = binds.len();
            let mut or_parts = vec![
                format!("lower(coalesce(email, '')) like ${n}"),
                format!("lower(coalesce(upstream_account_id, '')) like ${n}"),
                format!("id::text like ${n}"),
            ];
            // 够长就当作完整 token，用哈希精确命中
            if trimmed.len() >= MIN_TOKEN_LOOKUP_LEN {
                binds.push(Account::token_sha(trimmed));
                or_parts.push(format!("access_token_sha = ${}", binds.len()));
            }
            clauses.push(format!("({})", or_parts.join(" or ")));
        }
    }

    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        (format!("where {}", clauses.join(" and ")), binds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_sql_starts_empty() {
        let (sql, binds) = build_filter(&AccountFilter::default());
        assert_eq!(sql, "");
        assert!(binds.is_empty());
    }

    #[test]
    fn filter_sql_builds_placeholders() {
        let filter = AccountFilter {
            status: Some(AccountStatus::Normal),
            plan: Some(Plan::Plus),
            ..Default::default()
        };
        let (sql, binds) = build_filter(&filter);
        assert!(sql.starts_with("where "), "{sql}");
        assert!(sql.contains("status = $1"), "{sql}");
        assert!(sql.contains("plan = $2"), "{sql}");
        assert_eq!(binds, vec!["normal".to_string(), "plus".to_string()]);
    }

    #[test]
    fn keyword_lowercases_and_reuses_placeholder() {
        let filter = AccountFilter {
            keyword: Some("ABC".to_string()),
            ..Default::default()
        };
        let (sql, binds) = build_filter(&filter);
        assert_eq!(binds, vec!["%abc%".to_string()]);
        assert_eq!(sql.matches("$1").count(), 3, "三处都复用 $1: {sql}");
    }

    #[test]
    fn long_keyword_also_matches_token_sha() {
        let long_token = "a".repeat(40);
        let filter = AccountFilter {
            keyword: Some(long_token.clone()),
            ..Default::default()
        };
        let (sql, binds) = build_filter(&filter);
        assert!(sql.contains("access_token_sha = $2"), "{sql}");
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[1], Account::token_sha(&long_token));
    }

    #[test]
    fn blank_keyword_is_ignored() {
        let filter = AccountFilter {
            keyword: Some("   ".to_string()),
            ..Default::default()
        };
        let (sql, binds) = build_filter(&filter);
        assert_eq!(sql, "");
        assert!(binds.is_empty());
    }
}

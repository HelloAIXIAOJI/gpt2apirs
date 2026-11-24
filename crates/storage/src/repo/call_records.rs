//! 调用日志仓储。
//!
//! 记录每一次对外请求的路径、状态与耗时，给后台的日志页与监控页供数。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 一条待写入的调用记录。
#[derive(Debug, Clone)]
pub struct NewCallRecord {
    /// HTTP 方法。
    pub method: String,
    /// 请求路径。
    pub path: String,
    /// 状态码。
    pub status: i32,
    /// 耗时（毫秒）。
    pub duration_ms: i32,
    /// 关联账号（可为空）。
    pub account_id: Option<Uuid>,
    /// 请求 ID，便于和响应头对账。
    pub request_id: Option<Uuid>,
    /// 错误码（可为空）。
    pub error_code: Option<String>,
    /// 调用方 IP（取 X-Forwarded-For 第一跳，退 X-Real-IP）。
    pub client_ip: Option<String>,
    /// 调用方 UA。
    pub user_agent: Option<String>,
    /// 调用用的 API key 前缀（只留前缀，不落明文）。
    pub api_key_prefix: Option<String>,
    /// 这次失败的原因原文（详情页用；成功时为空）。
    pub error_message: Option<String>,
}

/// 查询到的一条记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CallRecordRow {
    /// 主键。
    pub id: i64,
    /// 时间。
    pub created_at: DateTime<Utc>,
    /// 方法。
    pub method: String,
    /// 路径。
    pub path: String,
    /// 状态码。
    pub status: i32,
    /// 耗时（毫秒）。
    pub duration_ms: i32,
    /// 账号。
    pub account_id: Option<Uuid>,
    /// 请求 ID。
    pub request_id: Option<Uuid>,
    /// 错误码。
    pub error_code: Option<String>,
    /// 调用方 IP。
    pub client_ip: Option<String>,
    /// 调用方 UA。
    pub user_agent: Option<String>,
    /// 调用用的 API key 前缀。
    pub api_key_prefix: Option<String>,
    /// 失败原因原文。
    pub error_message: Option<String>,
}

/// 日志查询条件。
#[derive(Debug, Clone, Default)]
pub struct CallRecordQuery {
    /// 状态码区间，`None` 表示不限。
    pub status_range: Option<(i32, i32)>,
    /// 路径前缀。
    pub path_prefix: Option<String>,
    /// 账号过滤。
    pub account_id: Option<Uuid>,
    /// 错误码（精确匹配）。
    pub error_code: Option<String>,
    /// 起始时间。
    pub since: Option<DateTime<Utc>>,
    /// 返回条数上限。
    pub limit: i64,
    /// 偏移。
    pub offset: i64,
}

/// 监控汇总。
#[derive(Debug, Clone, Default)]
pub struct CallSummary {
    /// 总数。
    pub total: i64,
    /// 成功数（2xx）。
    pub ok: i64,
    /// 失败数（非 2xx）。
    pub failed: i64,
    /// P50 耗时（毫秒）。
    pub p50_ms: Option<f64>,
    /// P95 耗时（毫秒）。
    pub p95_ms: Option<f64>,
    /// 错误码排行。
    pub top_errors: Vec<(String, i64)>,
}

/// 按天聚合的一个点。
#[derive(Debug, Clone, Default, serde::Serialize, sqlx::FromRow)]
pub struct DailyStat {
    /// 日期（YYYY-MM-DD，按服务器时区）。
    pub day: String,
    /// 请求总数。
    pub total: i64,
    /// 成功数（2xx）。
    pub ok: i64,
    /// 失败数（非 2xx）。
    pub failed: i64,
    /// 平均耗时（毫秒）。
    pub avg_ms: f64,
}

/// 排行榜的一项（路径 / 账号）。
#[derive(Debug, Clone, Default, serde::Serialize, sqlx::FromRow)]
pub struct RankedEntry {
    /// 分组键。
    pub key: String,
    /// 次数。
    pub count: i64,
}

/// 调用日志仓储。
#[derive(Debug, Clone)]
pub struct CallRecordRepo {
    pool: PgPool,
}

impl CallRecordRepo {
    /// 用连接池构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 写入一条。
    pub async fn insert(&self, record: &NewCallRecord) -> Result<(), StorageError> {
        sqlx::query(SQL_INSERT)
            .bind(&record.method)
            .bind(&record.path)
            .bind(record.status)
            .bind(record.duration_ms)
            .bind(record.account_id)
            .bind(record.request_id)
            .bind(&record.error_code)
            .bind(&record.client_ip)
            .bind(&record.user_agent)
            .bind(&record.api_key_prefix)
            .bind(&record.error_message)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 分页查询，按时间倒序。
    pub async fn list(&self, query: &CallRecordQuery) -> Result<Vec<CallRecordRow>, StorageError> {
        let (min_status, max_status) = query.status_range.unwrap_or((0, 999));
        let rows = sqlx::query_as::<_, CallRecordRow>(SQL_LIST)
            .bind(min_status)
            .bind(max_status)
            .bind(query.path_prefix.as_deref())
            .bind(query.account_id)
            .bind(query.since)
            .bind(query.limit.clamp(1, 500))
            .bind(query.offset.max(0))
            .bind(query.error_code.clone())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 同一筛选条件下的总条数（分页用）。
    pub async fn count(&self, query: &CallRecordQuery) -> Result<i64, StorageError> {
        let (min_status, max_status) = query.status_range.unwrap_or((0, 999));
        let total: i64 = sqlx::query_scalar(SQL_COUNT)
            .bind(min_status)
            .bind(max_status)
            .bind(query.path_prefix.as_deref())
            .bind(query.account_id)
            .bind(query.since)
            .bind(query.error_code.clone())
            .fetch_one(&self.pool)
            .await?;
        Ok(total)
    }

    /// 近 N 小时的汇总，给监控页。
    pub async fn summary(&self, hours: i64) -> Result<CallSummary, StorageError> {
        let window = format!("{} hours", hours.clamp(1, 24 * 30));
        let totals: (i64, i64, i64, Option<f64>, Option<f64>) = sqlx::query_as(SQL_SUMMARY)
            .bind(&window)
            .fetch_one(&self.pool)
            .await?;

        let errors: Vec<(String, i64)> = sqlx::query_as(SQL_TOP_ERRORS)
            .bind(&window)
            .fetch_all(&self.pool)
            .await?;

        Ok(CallSummary {
            total: totals.0,
            ok: totals.1,
            failed: totals.2,
            p50_ms: totals.3,
            p95_ms: totals.4,
            top_errors: errors,
        })
    }

    /// 按主键取一条（详情页用）。
    pub async fn get(&self, id: i64) -> Result<Option<CallRecordRow>, StorageError> {
        let row = sqlx::query_as::<_, CallRecordRow>(SQL_GET)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 删一条，返回是否真的删掉了（不存在时是 false，路由据此回 404）。
    pub async fn delete(&self, id: i64) -> Result<bool, StorageError> {
        let result = sqlx::query(SQL_DELETE).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// 删掉当前筛选条件下的全部记录，返回删除条数。
    ///
    /// 只认 `count` 用的那套筛选，`limit/offset` 不参与 —— 「删掉筛选结果」
    /// 必须是整批，不能删着删着只删掉当前页。
    pub async fn delete_matching(&self, query: &CallRecordQuery) -> Result<u64, StorageError> {
        let (min_status, max_status) = query.status_range.unwrap_or((0, 999));
        let result = sqlx::query(SQL_DELETE_MATCHED)
            .bind(min_status)
            .bind(max_status)
            .bind(query.path_prefix.as_deref())
            .bind(query.account_id)
            .bind(query.since)
            .bind(query.error_code.clone())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 删除早于 N 小时的记录，返回删除条数。
    pub async fn purge_older_than(&self, hours: i64) -> Result<u64, StorageError> {
        let window = format!("{} hours", hours.max(1));
        let result = sqlx::query(SQL_PURGE)
            .bind(&window)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 近 `days` 天按天聚合（缺失的天由调用方补齐）。
    pub async fn daily(&self, days: i64) -> Result<Vec<DailyStat>, StorageError> {
        let rows = sqlx::query_as::<_, DailyStat>(SQL_DAILY)
            .bind(days.clamp(1, 90))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 近 `hours` 小时按路径排行。
    pub async fn by_path(&self, hours: i64, limit: i64) -> Result<Vec<RankedEntry>, StorageError> {
        let rows = sqlx::query_as::<_, RankedEntry>(SQL_BY_PATH)
            .bind(hours.clamp(1, 24 * 30))
            .bind(limit.clamp(1, 50))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 近 `hours` 小时按账号排行。
    pub async fn by_account(&self, hours: i64, limit: i64) -> Result<Vec<RankedEntry>, StorageError> {
        let rows = sqlx::query_as::<_, RankedEntry>(SQL_BY_ACCOUNT)
            .bind(hours.clamp(1, 24 * 30))
            .bind(limit.clamp(1, 50))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

}

const SQL_COUNT: &str = r#"select count(*)
   from call_records
   where status between $1 and $2
     and ($3::text is null or path like $3 || '%')
     and ($4::uuid is null or account_id = $4)
     and ($5::timestamptz is null or created_at >= $5)
     and ($6::text is null or error_code = $6)"#;

const SQL_INSERT: &str = r#"insert into call_records
   (method, path, status, duration_ms, account_id, request_id, error_code,
    client_ip, user_agent, api_key_prefix, error_message)
   values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#;

const SQL_LIST: &str = r#"select id, created_at, method, path, status, duration_ms, account_id, request_id, error_code,
          client_ip, user_agent, api_key_prefix, error_message
   from call_records
   where status between $1 and $2
     and ($3::text is null or path like $3 || '%')
     and ($4::uuid is null or account_id = $4)
     and ($5::timestamptz is null or created_at >= $5)
     and ($8::text is null or error_code = $8)
   order by created_at desc
   limit $6 offset $7"#;

const SQL_SUMMARY: &str = r#"select count(*),
          count(*) filter (where status between 200 and 299),
          count(*) filter (where status not between 200 and 299),
          percentile_cont(0.5) within group (order by duration_ms),
          percentile_cont(0.95) within group (order by duration_ms)
   from call_records
   where created_at >= now() - $1::interval"#;

const SQL_TOP_ERRORS: &str = r#"select coalesce(error_code, status::text) as code, count(*) as hits
   from call_records
   where created_at >= now() - $1::interval and status not between 200 and 299
   group by code order by hits desc limit 5"#;

const SQL_PURGE: &str = r#"delete from call_records where created_at < now() - $1::interval"#;

/// 单条详情。
const SQL_GET: &str = r#"select id, created_at, method, path, status, duration_ms, account_id, request_id, error_code,
          client_ip, user_agent, api_key_prefix, error_message
   from call_records where id = $1"#;

/// 删单条。
const SQL_DELETE: &str = "delete from call_records where id = $1";

/// 按筛选批量删（与 SQL_COUNT 同一套条件，参数顺序也要一致）。
const SQL_DELETE_MATCHED: &str = r#"delete from call_records
   where status between $1 and $2
     and ($3::text is null or path like $3 || '%')
     and ($4::uuid is null or account_id = $4)
     and ($5::timestamptz is null or created_at >= $5)
     and ($6::text is null or error_code = $6)"#;
/// 按天聚合。
const SQL_DAILY: &str = r#"
    select to_char(date_trunc('day', created_at), 'YYYY-MM-DD') as day,
           count(*) as total,
           count(*) filter (where status between 200 and 299) as ok,
           count(*) filter (where status not between 200 and 299) as failed,
           coalesce(avg(duration_ms), 0)::float8 as avg_ms
    from call_records
    where created_at >= now() - make_interval(days => $1::int)
    group by 1
    order by 1
"#;

/// 按路径排行。
const SQL_BY_PATH: &str = r#"
    select path as key, count(*) as count
    from call_records
    where created_at >= now() - make_interval(hours => $1::int)
    group by 1
    order by 2 desc
    limit $2
"#;

/// 按账号排行。
const SQL_BY_ACCOUNT: &str = r#"
    select account_id::text as key, count(*) as count
    from call_records
    where account_id is not null
      and created_at >= now() - make_interval(hours => $1::int)
    group by 1
    order by 2 desc
    limit $2
"#;
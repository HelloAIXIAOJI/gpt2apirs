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

    /// 删除早于 N 小时的记录，返回删除条数。
    pub async fn purge_older_than(&self, hours: i64) -> Result<u64, StorageError> {
        let window = format!("{} hours", hours.max(1));
        let result = sqlx::query(SQL_PURGE)
            .bind(&window)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

const SQL_COUNT: &str = r#"select count(*)
   from call_records
   where status between $1 and $2
     and ($3::text is null or path like $3 || '%')
     and ($4::uuid is null or account_id = $4)
     and ($5::timestamptz is null or created_at >= $5)
     and ($6::text is null or error_code = $6)"#;

const SQL_INSERT: &str = "insert into call_records (method, path, status, duration_ms, account_id, request_id, error_code) values ($1, $2, $3, $4, $5, $6, $7)";

const SQL_LIST: &str = r#"select id, created_at, method, path, status, duration_ms, account_id, request_id, error_code
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

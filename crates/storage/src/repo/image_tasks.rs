//! 异步任务仓储：任务落库，重启后历史可查。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::pool::StorageError;

/// 任务结果里的一张图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskImageRow {
    /// 相对数据目录的路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 宽。
    pub width: i32,
    /// 高。
    pub height: i32,
}

/// 一条任务记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImageTaskRow {
    /// 主键。
    pub id: Uuid,
    /// 状态。
    pub status: String,
    /// 提示词。
    pub prompt: String,
    /// 对外模型名。
    pub model: String,
    /// 请求张数。
    pub n: i32,
    /// 返回格式。
    pub response_format: String,
    /// 输出格式。
    pub output_format: Option<String>,
    /// GIF 帧数。
    pub frame_count: Option<i32>,
    /// GIF 帧时长。
    pub frame_duration_ms: Option<i32>,
    /// 对外基础地址。
    pub base_url: String,
    /// 当前阶段。
    pub progress: Option<String>,
    /// 耗时（毫秒）。
    pub duration_ms: Option<i64>,
    /// 输入 token。
    pub input_tokens: Option<i64>,
    /// 输出 token。
    pub output_tokens: Option<i64>,
    /// 总 token。
    pub total_tokens: Option<i64>,
    /// 失败原因。
    pub error: Option<String>,
    /// 结果图片。
    pub images: Value,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
    /// 完成时间。
    pub finished_at: Option<DateTime<Utc>>,
}

/// 图片任务按天聚合的一点。
#[derive(Debug, Clone, Default, serde::Serialize, sqlx::FromRow)]
pub struct ImageDailyStat {
    /// 日期（YYYY-MM-DD）。
    pub day: String,
    /// 新建任务数。
    pub tasks: i64,
    /// 成功数。
    pub done: i64,
    /// 失败数。
    pub failed: i64,
    /// 产出的图片张数。
    pub images: i64,
    /// 总 token。
    pub tokens: i64,
}

/// 按模型的排行项。
#[derive(Debug, Clone, Default, serde::Serialize, sqlx::FromRow)]
pub struct ModelStat {
    /// 模型名。
    pub model: String,
    /// 任务数。
    pub tasks: i64,
    /// 图片张数。
    pub images: i64,
}

/// 新任务。
#[derive(Debug, Clone)]
pub struct NewImageTask {
    /// 主键。
    pub id: Uuid,
    /// 提示词。
    pub prompt: String,
    /// 模型。
    pub model: String,
    /// 张数。
    pub n: i32,
    /// 返回格式。
    pub response_format: String,
    /// 输出格式。
    pub output_format: Option<String>,
    /// GIF 帧数。
    pub frame_count: Option<i32>,
    /// GIF 帧时长。
    pub frame_duration_ms: Option<i32>,
    /// 基础地址。
    pub base_url: String,
}

const COLUMNS: &str = "id, status, prompt, model, n, response_format, output_format, frame_count, frame_duration_ms, base_url, progress, duration_ms, input_tokens, output_tokens, total_tokens, error, images, created_at, updated_at, finished_at";

/// 任务仓储。
#[derive(Debug, Clone)]
pub struct ImageTaskRepo {
    pool: PgPool,
}

impl ImageTaskRepo {
    /// 新建。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 写入新任务。
    pub async fn insert(&self, task: &NewImageTask) -> Result<(), StorageError> {
        sqlx::query(
            r#"insert into image_tasks
                (id, status, prompt, model, n, response_format, output_format,
                 frame_count, frame_duration_ms, base_url, progress)
               values ($1, 'queued', $2, $3, $4, $5, $6, $7, $8, $9, '排队中')"#,
        )
        .bind(task.id)
        .bind(&task.prompt)
        .bind(&task.model)
        .bind(task.n)
        .bind(&task.response_format)
        .bind(&task.output_format)
        .bind(task.frame_count)
        .bind(task.frame_duration_ms)
        .bind(&task.base_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 置为运行中；已被删除或不是 queued 时返回 false。
    pub async fn mark_running(&self, id: Uuid, progress: &str) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"update image_tasks set status = 'running', progress = $2, updated_at = now()
               where id = $1 and status = 'queued'"#,
        )
        .bind(id)
        .bind(progress)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 更新阶段文案。
    pub async fn set_progress(&self, id: Uuid, progress: &str) -> Result<(), StorageError> {
        sqlx::query("update image_tasks set progress = $2, updated_at = now() where id = $1")
            .bind(id)
            .bind(progress)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 结束任务。
    #[allow(clippy::too_many_arguments)]
    pub async fn finish(
        &self,
        id: Uuid,
        succeeded: bool,
        images: &[TaskImageRow],
        error: Option<&str>,
        duration_ms: i64,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Result<(), StorageError> {
        let payload = serde_json::to_value(images).map_err(|error| {
            StorageError::Query(sqlx::Error::Protocol(format!("序列化任务图片失败：{error}")))
        })?;
        sqlx::query(
            r#"update image_tasks set
                 status = $2, progress = $3, images = $4, error = $5,
                 duration_ms = $6, input_tokens = $7, output_tokens = $8,
                 total_tokens = $7 + $8, finished_at = now(), updated_at = now()
               where id = $1"#,
        )
        .bind(id)
        .bind(if succeeded { "succeeded" } else { "failed" })
        .bind(if succeeded { "已完成" } else { "已失败" })
        .bind(payload)
        .bind(error)
        .bind(duration_ms)
        .bind(input_tokens)
        .bind(output_tokens)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除任务。
    pub async fn delete(&self, id: Uuid) -> Result<bool, StorageError> {
        let result = sqlx::query("delete from image_tasks where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 最近若干条（新到旧）。
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<ImageTaskRow>, StorageError> {
        let sql = format!("select {COLUMNS} from image_tasks order by created_at desc limit $1");
        let rows = sqlx::query_as::<_, ImageTaskRow>(&sql)
            .bind(limit.clamp(1, 500))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 把上次进程留下的未完成任务标成失败，返回受影响行数。
    pub async fn fail_unfinished(&self, reason: &str) -> Result<u64, StorageError> {
        let result = sqlx::query(
            r#"update image_tasks set status = 'failed', progress = '已中断', error = $1,
                 finished_at = now(), updated_at = now()
               where status in ('queued', 'running')"#,
        )
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 清掉过期的终态任务，返回删除行数。
    pub async fn purge_terminal_older_than(&self, hours: i64) -> Result<u64, StorageError> {
        let result = sqlx::query(
            "delete from image_tasks where status in ('succeeded', 'failed') and created_at < now() - make_interval(hours => $1::int)",
        )
        .bind(hours.clamp(1, 24 * 365) as i32)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 近 `days` 天按天聚合。
    pub async fn daily(&self, days: i64) -> Result<Vec<ImageDailyStat>, StorageError> {
        let rows = sqlx::query_as::<_, ImageDailyStat>(SQL_DAILY)
            .bind(days.clamp(1, 90))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 近 `days` 天按模型排行。
    pub async fn by_model(&self, days: i64, limit: i64) -> Result<Vec<ModelStat>, StorageError> {
        let rows = sqlx::query_as::<_, ModelStat>(SQL_BY_MODEL)
            .bind(days.clamp(1, 90))
            .bind(limit.clamp(1, 50))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }
}

/// 按天聚合。
const SQL_DAILY: &str = r#"
    select to_char(date_trunc('day', created_at), 'YYYY-MM-DD') as day,
           count(*) as tasks,
           count(*) filter (where status = 'succeeded') as done,
           count(*) filter (where status = 'failed') as failed,
           coalesce(sum(jsonb_array_length(images)), 0)::bigint as images,
           coalesce(sum(total_tokens), 0)::bigint as tokens
    from image_tasks
    where created_at >= now() - make_interval(days => $1::int)
    group by 1
    order by 1
"#;

/// 按模型排行。
const SQL_BY_MODEL: &str = r#"
    select model, count(*) as tasks,
           coalesce(sum(jsonb_array_length(images)), 0)::bigint as images
    from image_tasks
    where created_at >= now() - make_interval(days => $1::int)
    group by 1
    order by 2 desc
    limit $2
"#;
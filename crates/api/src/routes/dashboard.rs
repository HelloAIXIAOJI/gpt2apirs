//! 仪表盘聚合：近 N 天的调用/图片趋势与排行。
//!
//! 只做聚合查询，不落表；缺数据的天由 fill_days 补齐，
//! 前端拿到的是等长序列，画图不用自己补空。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{Duration, Utc};
use gpt2apirs_core::AppError;
use gpt2apirs_storage::repo::{CallRecordRepo, DailyStat, ImageDailyStat, ImageTaskRepo};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

/// 查询参数。
#[derive(Debug, Deserialize)]
pub struct DashboardQuery {
    /// 趋势天数（默认 7，最多 90）。
    #[serde(default)]
    pub days: Option<i64>,
    /// 排行窗口小时数（默认 24）。
    #[serde(default)]
    pub hours: Option<i64>,
    /// 排行条数（默认 5）。
    #[serde(default)]
    pub top: Option<i64>,
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::admin(AppError::Internal(anyhow::anyhow!("{error}")))
}

/// 补齐缺失的日期。
///
/// 聚合 SQL 只返回有数据的天；画图需要等长序列，所以按「今天往前 N 天」补零。
/// 纯函数，便于单测。
pub fn fill_days(mut rows: Vec<DailyStat>, days: i64, today: chrono::NaiveDate) -> Vec<DailyStat> {
    let days = days.clamp(1, 90);
    let existing: std::collections::HashMap<String, DailyStat> =
        rows.drain(..).map(|row| (row.day.clone(), row)).collect();
    let mut out = Vec::with_capacity(days as usize);
    for offset in (0..days).rev() {
        let day = (today - Duration::days(offset)).format("%Y-%m-%d").to_string();
        out.push(existing.get(&day).cloned().unwrap_or(DailyStat {
            day,
            ..DailyStat::default()
        }));
    }
    out
}

/// 给图片序列补齐日期（同上）。
pub fn fill_image_days(
    mut rows: Vec<ImageDailyStat>,
    days: i64,
    today: chrono::NaiveDate,
) -> Vec<ImageDailyStat> {
    let days = days.clamp(1, 90);
    let existing: std::collections::HashMap<String, ImageDailyStat> =
        rows.drain(..).map(|row| (row.day.clone(), row)).collect();
    let mut out = Vec::with_capacity(days as usize);
    for offset in (0..days).rev() {
        let day = (today - Duration::days(offset)).format("%Y-%m-%d").to_string();
        out.push(existing.get(&day).cloned().unwrap_or(ImageDailyStat {
            day,
            ..ImageDailyStat::default()
        }));
    }
    out
}

/// GET /api/dashboard —— 趋势 + 排行，一次拿全。
pub async fn dashboard(
    State(state): State<Arc<AppState>>,
    Query(params): Query<DashboardQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = params.days.unwrap_or(7).clamp(1, 90);
    let hours = params.hours.unwrap_or(24).clamp(1, 24 * 90);
    let top = params.top.unwrap_or(5).clamp(1, 20);

    let calls = CallRecordRepo::new(state.db.clone());
    let tasks = ImageTaskRepo::new(state.db.clone());

    let today = Utc::now().date_naive();
    let call_daily = fill_days(calls.daily(days).await.map_err(internal)?, days, today);
    let image_daily = fill_image_days(tasks.daily(days).await.map_err(internal)?, days, today);
    let by_path = calls.by_path(hours, top).await.map_err(internal)?;
    let by_account = calls.by_account(hours, top).await.map_err(internal)?;
    let by_model = tasks.by_model(days, top).await.map_err(internal)?;

    let total_calls: i64 = call_daily.iter().map(|row| row.total).sum();
    let total_ok: i64 = call_daily.iter().map(|row| row.ok).sum();
    let total_failed: i64 = call_daily.iter().map(|row| row.failed).sum();
    let total_tasks: i64 = image_daily.iter().map(|row| row.tasks).sum();
    let total_images: i64 = image_daily.iter().map(|row| row.images).sum();

    Ok(Json(json!({
        "range_days": days,
        "rank_hours": hours,
        "calls": {
            "daily": call_daily,
            "total": total_calls,
            "ok": total_ok,
            "failed": total_failed,
            "by_path": by_path,
            "by_account": by_account,
        },
        "images": {
            "daily": image_daily,
            "tasks": total_tasks,
            "images": total_images,
            "by_model": by_model,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).expect("合法日期")
    }

    #[test]
    fn missing_days_are_filled_in_order() {
        let today = day(2026, 11, 24);
        let rows = vec![DailyStat {
            day: "2026-11-22".to_string(),
            total: 5,
            ok: 4,
            failed: 1,
            avg_ms: 120.0,
        }];
        let filled = fill_days(rows, 3, today);
        assert_eq!(filled.len(), 3);
        assert_eq!(filled[0].day, "2026-11-22");
        assert_eq!(filled[0].total, 5);
        assert_eq!(filled[1].day, "2026-11-23");
        assert_eq!(filled[1].total, 0);
        assert_eq!(filled[2].day, "2026-11-24");
    }

    #[test]
    fn image_days_are_filled_too() {
        let today = day(2026, 11, 24);
        let filled = fill_image_days(Vec::new(), 2, today);
        assert_eq!(filled.len(), 2);
        assert!(filled.iter().all(|row| row.tasks == 0 && row.images == 0));
        assert_eq!(filled[1].day, "2026-11-24");
    }

    #[test]
    fn days_are_clamped() {
        let today = day(2026, 11, 24);
        assert_eq!(fill_days(Vec::new(), 0, today).len(), 1);
        assert_eq!(fill_days(Vec::new(), 999, today).len(), 90);
    }
}

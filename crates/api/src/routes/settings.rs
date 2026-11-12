//! `/api/settings` —— 运行层设置的读写。
//!
//! # 为什么这个端点必须存在

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use gpt2apirs_core::settings::{
    NUMERIC_SPECS, NumericKind, STRING_SPECS, normalize_f64, normalize_i64, normalize_string,
    numeric_spec, string_spec,
};
use gpt2apirs_core::{AppError, settings::NumericSpec};
use gpt2apirs_storage::repo::PgSettingsRepo;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::ApiError;
use crate::state::AppState;

/// `PUT /api/settings` 的请求体。
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    /// 要修改的键值对（只传要改的）。
    pub values: Map<String, Value>,
}

fn numeric_entry(spec: &NumericSpec, current: &Value) -> Value {
    let value = match spec.kind {
        NumericKind::Integer => json!(normalize_i64(spec, as_number(current))),
        NumericKind::Float => json!(normalize_f64(spec, as_number(current))),
    };
    json!({
        "key": spec.key,
        "value": value,
        "default": spec.default,
        "min": spec.min,
        "max": spec.max,
        "kind": match spec.kind {
            NumericKind::Integer => "integer",
            NumericKind::Float => "float",
        },
        "unit": spec.unit,
        "restart_required": spec.restart_required,
        "description": spec.description,
    })
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn build_payload(state: &AppState) -> Value {
    let snapshot = state.settings.snapshot();
    let numerics: Vec<Value> = NUMERIC_SPECS
        .iter()
        .map(|spec| {
            let current = snapshot.get(spec.key).cloned().unwrap_or(json!(spec.default));
            numeric_entry(spec, &current)
        })
        .collect();
    let strings: Vec<Value> = STRING_SPECS
        .iter()
        .map(|spec| {
            let current = snapshot
                .get(spec.key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| spec.default.to_string());
            json!({
                "key": spec.key,
                "value": normalize_string(spec, &current),
                "default": spec.default,
                "max_len": spec.max_len,
                "restart_required": spec.restart_required,
                "description": spec.description,
            })
        })
        .collect();
    json!({
        "numerics": numerics,
        "strings": strings,
        "updated_at": state.settings.updated_at().map(|stamp| stamp.to_rfc3339()),
    })
}

/// `GET /api/settings`
pub async fn get_settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(build_payload(&state))
}

/// `PUT /api/settings`
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<UpdateSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.values.is_empty() {
        return Err(ApiError::admin(AppError::invalid_param(
            "values 不能为空",
            "values",
        )));
    }

    // ---- 先全校验 ----
    let mut normalized: HashMap<String, Value> = HashMap::new();
    for (key, value) in &request.values {
        if let Some(spec) = numeric_spec(key) {
            let Some(number) = as_number(value) else {
                return Err(ApiError::admin(AppError::invalid_param(
                    format!("{key} 需要数值，收到 {value}"),
                    key,
                )));
            };
            let applied = match spec.kind {
                NumericKind::Integer => json!(normalize_i64(spec, Some(number))),
                NumericKind::Float => json!(normalize_f64(spec, Some(number))),
            };
            normalized.insert(key.clone(), applied);
            continue;
        }
        if let Some(spec) = string_spec(key) {
            let Some(text) = as_text(value) else {
                return Err(ApiError::admin(AppError::invalid_param(
                    format!("{key} 需要字符串，收到 {value}"),
                    key,
                )));
            };
            normalized.insert(key.clone(), json!(normalize_string(spec, &text)));
            continue;
        }
        // 未知键：**拒收而不是忽略**。默默忽略会让用户以为设成功了。
        return Err(ApiError::admin(AppError::InvalidRequest {
            message: format!(
                "未知设置项 {key}；可用项见 GET /api/settings（{} 项数值 + {} 项字符串）",
                NUMERIC_SPECS.len(),
                STRING_SPECS.len()
            ),
            param: Some(key.clone()),
            code: Some("unknown_setting_key".to_string()),
        }));
    }

    // ---- 再落库 ----
    let repo = PgSettingsRepo::new(state.db.clone());
    let snapshot = repo.merge(&normalized).await.map_err(|error| {
        ApiError::admin(AppError::Internal(anyhow::anyhow!(
            "写入设置失败：{error}"
        )))
    })?;

    // ---- 热重载 ----
    state
        .settings
        .replace(snapshot.data.clone(), snapshot.updated_at);

    tracing::info!(
        keys = normalized.keys().cloned().collect::<Vec<_>>().join(","),
        "设置已更新"
    );

    Ok(Json(build_payload(&state)))
}

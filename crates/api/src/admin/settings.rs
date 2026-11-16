//! 设置页：按域分组的运行设置表单。
//!
//! 字段清单直接来自 `gpt2apirs_core::settings` 的规格表，页面不维护第二份定义；
//! 提交复用 `/api/settings` 的处理函数，校验规则只有一处。

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use gpt2apirs_core::settings::{NUMERIC_SPECS, NumericKind, STRING_SPECS};
use serde_json::{Map, Value, json};

use super::has_session;
use super::nav;
use crate::state::AppState;

/// 一个设置字段。
pub struct Field {
    /// 键名。
    pub key: &'static str,
    /// 说明（取自规格表）。
    pub label: &'static str,
    /// 当前值。
    pub value: String,
    /// 是否是长文本（用 textarea）。
    pub long_text: bool,
    /// 取值范围或长度上限提示。
    pub hint: String,
    /// 改完是否需要重启。
    pub restart: bool,
}

/// 一组设置。
pub struct Group {
    /// 组标题。
    pub title: &'static str,
    /// 组内字段。
    pub fields: Vec<Field>,
}

/// 设置页。
#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    nav: Vec<nav::NavItem>,
    groups: Vec<Group>,
    flash: String,
    updated_at: String,
}

/// 键名归到哪个组。
fn group_of(key: &str) -> &'static str {
    if key.starts_with("image_") {
        "生图"
    } else if key.starts_with("text_") {
        "文本"
    } else if key.starts_with("account_") {
        "账号池"
    } else if key.starts_with("proxy_") {
        "代理出口"
    } else if key.starts_with("log_") || key.starts_with("console_") {
        "日志与运维"
    } else if key.starts_with("chat_") {
        "对话"
    } else {
        "其它"
    }
}

fn groups_of(state: &AppState) -> Vec<Group> {
    let snapshot = state.settings.snapshot();
    let mut groups: Vec<Group> = Vec::new();
    let mut push = |title: &'static str, field: Field| {
        match groups.iter_mut().find(|group| group.title == title) {
            Some(group) => group.fields.push(field),
            None => groups.push(Group {
                title,
                fields: vec![field],
            }),
        }
    };

    for spec in NUMERIC_SPECS {
        let current = snapshot.get(spec.key).cloned().unwrap_or(json!(spec.default));
        // 整数项按整数显示：JSON 里存的是 5000.0，直接显示会让人以为它是小数。
        let value = match (spec.kind, &current) {
            (NumericKind::Integer, Value::Number(number)) => number
                .as_i64()
                .map(|value| value.to_string())
                .or_else(|| number.as_f64().map(|value| (value as i64).to_string()))
                .unwrap_or_default(),
            (_, Value::Number(number)) => number.to_string(),
            (_, Value::String(text)) => text.clone(),
            (_, other) => other.to_string(),
        };
        let kind_hint = match spec.kind {
            NumericKind::Integer => format!("整数 {}..={}", spec.min, spec.max.map(|max| max.to_string()).unwrap_or_else(|| "∞".to_string())),
            NumericKind::Float => format!("小数 {}..={}", spec.min, spec.max.map(|max| max.to_string()).unwrap_or_else(|| "∞".to_string())),
        };
        let unit = spec.unit.map(|unit| format!("（{unit}）")).unwrap_or_default();
        push(
            group_of(spec.key),
            Field {
                key: spec.key,
                label: spec.description,
                value,
                long_text: false,
                hint: format!("{kind_hint}{unit}，默认 {}", spec.default),
                restart: spec.restart_required,
            },
        );
    }

    for spec in STRING_SPECS {
        let value = snapshot
            .get(spec.key)
            .and_then(Value::as_str)
            .unwrap_or(spec.default)
            .to_string();
        push(
            group_of(spec.key),
            Field {
                key: spec.key,
                label: spec.description,
                value,
                long_text: spec.max_len >= 200,
                hint: format!("最多 {} 个字符", spec.max_len),
                restart: spec.restart_required,
            },
        );
    }

    groups
}

/// `GET /settings`
pub async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<super::accounts::FlashQuery>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    render(SettingsPage {
        nav: nav::items("/settings"),
        groups: groups_of(&state),
        flash: query.flash,
        updated_at: state
            .settings
            .updated_at()
            .map(|stamp| stamp.to_rfc3339())
            .unwrap_or_default(),
    })
}

/// `POST /settings`
pub async fn save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if !has_session(&state, &headers) {
        return Redirect::to("/").into_response();
    }
    let mut values = Map::new();
    for (key, raw) in form {
        let trimmed = raw.trim();
        if let Some(spec) = gpt2apirs_core::settings::numeric_spec(&key) {
            let parsed = match spec.kind {
                NumericKind::Integer => trimmed
                    .parse::<i64>()
                    .map(|value| json!(value))
                    .unwrap_or_else(|_| json!(spec.default)),
                NumericKind::Float => trimmed
                    .parse::<f64>()
                    .map(|value| json!(value))
                    .unwrap_or_else(|_| json!(spec.default)),
            };
            values.insert(key, parsed);
        } else {
            values.insert(key, json!(trimmed));
        }
    }
    let request = crate::routes::settings::UpdateSettingsRequest { values };
    match crate::routes::settings::update_settings(State(state.clone()), axum::Json(request)).await {
        Ok(_) => Redirect::to(&format!("/settings?flash={}", super::accounts::encode("设置已保存"))).into_response(),
        Err(error) => {
            let message = format!("保存失败：{error:?}");
            let encoded = super::accounts::encode(&message);
            Redirect::to(&format!("/settings?flash={encoded}")).into_response()
        }
    }
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("模板渲染失败：{error}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_group_by_key_prefix() {
        assert_eq!(group_of("image_poll_timeout_secs"), "生图");
        assert_eq!(group_of("text_max_account_attempts"), "文本");
        assert_eq!(group_of("account_processing_concurrency"), "账号池");
        assert_eq!(group_of("proxy_enabled"), "代理出口");
        assert_eq!(group_of("log_retention_hours"), "日志与运维");
        assert_eq!(group_of("chat_blocked_keywords"), "对话");
        assert_eq!(group_of("refresh_account_interval_minute"), "其它");
    }

    #[test]
    fn page_renders_fields_with_hints_and_restart_badge() {
        let page = SettingsPage {
            nav: nav::items("/settings"),
            groups: vec![Group {
                title: "代理出口",
                fields: vec![
                    Field {
                        key: "proxy_enabled",
                        label: "是否启用订阅节点作为全局出口",
                        value: "1".to_string(),
                        long_text: false,
                        hint: "整数 0..=1，默认 0".to_string(),
                        restart: true,
                    },
                    Field {
                        key: "proxy_subscription_url",
                        label: "订阅地址",
                        value: "https://example.com/sub".to_string(),
                        long_text: true,
                        hint: "最多 1024 个字符".to_string(),
                        restart: false,
                    },
                ],
            }],
            flash: "设置已保存".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let html = page.render().expect("渲染设置页");
        assert!(html.contains("proxy_enabled"), "{html}");
        assert!(html.contains("需重启"), "{html}");
        assert!(html.contains("设置已保存"), "{html}");
        assert!(html.contains("<textarea"), "长文本字段用 textarea：{html}");
        assert!(html.contains("value=\"1\""), "{html}");
    }
}

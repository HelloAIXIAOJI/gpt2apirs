//! 运行层设置的**内存快照**。
//!
//! 数据库里是「单行 + 一个大 JSONB」，这里把它装进可原子替换的快照，
//! 读路径无锁竞争（`RwLock` + `Arc`），写入时整体替换。
//!
//! # 值的三层来源
//!
//! ```text
//! .env（引导层）  >  数据库 system_settings（运行层）  >  契约表默认值
//! ```
//!
//! 本模块只管后两层：先按数据库值，缺了就用 `spec` 里的默认值。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::spec::{NumericKind, NumericSpec, normalize_f64, normalize_i64, numeric_spec};

/// 一项设置的元数据 + 当前值，供管理后台渲染表单。
#[derive(Debug, Clone)]
pub struct FieldMeta {
    /// 键名。
    pub key: &'static str,
    /// 类型。
    pub kind: NumericKind,
    /// 默认值。
    pub default: f64,
    /// 下限。
    pub min: f64,
    /// 上限。
    pub max: Option<f64>,
    /// 单位。
    pub unit: Option<&'static str>,
    /// 是否需重启。
    pub restart_required: bool,
    /// 说明。
    pub description: &'static str,
    /// 当前生效值。
    pub current: f64,
}

/// 运行层设置快照。
#[derive(Debug, Default)]
pub struct RuntimeSettings {
    values: RwLock<Arc<HashMap<String, Value>>>,
    updated_at: RwLock<Option<DateTime<Utc>>>,
}

impl RuntimeSettings {
    /// 用一份原始 map 构造（不校验，只在读取时按契约归一化）。
    pub fn new(values: HashMap<String, Value>) -> Self {
        Self {
            values: RwLock::new(Arc::new(values)),
            updated_at: RwLock::new(None),
        }
    }

    /// 当前快照（读一次拿到的 Arc 在一次请求内保持一致）。
    pub fn snapshot(&self) -> Arc<HashMap<String, Value>> {
        self.values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 最近一次更新时间。
    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        *self
            .updated_at
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 整体替换快照（热重载入口）。
    pub fn replace(&self, values: HashMap<String, Value>, updated_at: Option<DateTime<Utc>>) {
        *self
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(values);
        *self
            .updated_at
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = updated_at;
    }

    /// 取整数设置（按契约钳制）。
    ///
    /// 未知 key 返回 0 并给出 warn 日志——这是**内部错误**，不是用户输入错误。
    pub fn get_i64(&self, key: &str) -> i64 {
        let Some(spec) = numeric_spec(key) else {
            tracing::warn!("请求了未定义的设置项: {key}");
            return 0;
        };
        normalize_i64(spec, self.raw_number(key))
    }

    /// 取浮点设置（按契约钳制）。
    pub fn get_f64(&self, key: &str) -> f64 {
        let Some(spec) = numeric_spec(key) else {
            tracing::warn!("请求了未定义的设置项: {key}");
            return 0.0;
        };
        normalize_f64(spec, self.raw_number(key))
    }

    /// 取布尔设置。缺省 `false`。
    pub fn get_bool(&self, key: &str) -> bool {
        match self.lookup(key) {
            Some(Value::Bool(b)) => b,
            Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            Some(Value::Number(n)) => n.as_f64().map(|v| v != 0.0).unwrap_or(false),
            _ => false,
        }
    }

    /// 取字符串设置。缺省空串。
    pub fn get_string(&self, key: &str) -> String {
        match self.lookup(key) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    /// 生成全部数值项的元数据（管理后台用）。
    pub fn metadata(&self) -> Vec<FieldMeta> {
        super::spec::NUMERIC_SPECS
            .iter()
            .map(|spec| FieldMeta {
                key: spec.key,
                kind: spec.kind,
                default: spec.default,
                min: spec.min,
                max: spec.max,
                unit: spec.unit,
                restart_required: spec.restart_required,
                description: spec.description,
                current: normalize_f64(spec, self.raw_number(spec.key)),
            })
            .collect()
    }

    /// 取出原始 JSON 值。兼容两种存法：
    ///
    /// - 裸值：`{"image_poll_timeout_secs": 60}`
    /// - 带包装：`{"image_poll_timeout_secs": {"v": 60}}`（为将来存元信息留余地）
    fn lookup(&self, key: &str) -> Option<Value> {
        let snapshot = self.snapshot();
        let raw = snapshot.get(key)?;
        match raw {
            Value::Object(map) => map.get("v").cloned(),
            other => Some(other.clone()),
        }
    }

    /// 把原始值解成数字（字符串数字也接受）。
    fn raw_number(&self, key: &str) -> Option<f64> {
        match self.lookup(key)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

impl NumericSpec {
    /// 便捷：本规格的默认值。
    pub fn default_value(&self) -> f64 {
        self.default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: Vec<(&str, Value)>) -> HashMap<String, Value> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn missing_values_fall_back_to_defaults() {
        let settings = RuntimeSettings::new(HashMap::new());
        assert_eq!(settings.get_i64("image_poll_timeout_secs"), 60);
        assert_eq!(settings.get_f64("image_poll_interval_secs"), 5.0);
        assert!(!settings.get_bool("any_bool"));
        assert_eq!(settings.get_string("any_str"), "");
    }

    #[test]
    fn stored_values_are_clamped() {
        let settings = RuntimeSettings::new(map(vec![
            ("image_account_concurrency", json!(99)),
            ("image_poll_interval_secs", json!(0.1)),
        ]));
        assert_eq!(settings.get_i64("image_account_concurrency"), 3);
        assert_eq!(settings.get_f64("image_poll_interval_secs"), 0.5);
    }

    #[test]
    fn string_numbers_are_accepted() {
        let settings = RuntimeSettings::new(map(vec![
            ("image_poll_timeout_secs", json!("120")),
        ]));
        assert_eq!(settings.get_i64("image_poll_timeout_secs"), 120);
    }

    #[test]
    fn wrapped_values_are_supported() {
        let settings = RuntimeSettings::new(map(vec![
            ("image_settle_secs", json!({"v": 2.5})),
        ]));
        assert_eq!(settings.get_f64("image_settle_secs"), 2.5);
    }

    #[test]
    fn bool_accepts_multiple_representations() {
        let settings = RuntimeSettings::new(map(vec![
            ("a", json!(true)),
            ("b", json!("true")),
            ("c", json!("no")),
            ("d", json!(1)),
            ("e", json!(0)),
        ]));
        assert!(settings.get_bool("a"));
        assert!(settings.get_bool("b"));
        assert!(!settings.get_bool("c"));
        assert!(settings.get_bool("d"));
        assert!(!settings.get_bool("e"));
    }

    #[test]
    fn unknown_key_returns_zero() {
        let settings = RuntimeSettings::new(HashMap::new());
        assert_eq!(settings.get_i64("no_such_setting"), 0);
        assert_eq!(settings.get_f64("no_such_setting"), 0.0);
    }

    #[test]
    fn replace_swaps_snapshot() {
        let settings = RuntimeSettings::new(HashMap::new());
        assert_eq!(settings.get_i64("image_poll_timeout_secs"), 60);

        settings.replace(map(vec![("image_poll_timeout_secs", json!(120))]), Some(Utc::now()));
        assert_eq!(settings.get_i64("image_poll_timeout_secs"), 120);
        assert!(settings.updated_at().is_some());
    }

    #[test]
    fn metadata_covers_all_numeric_specs() {
        let settings = RuntimeSettings::new(map(vec![
            ("image_account_concurrency", json!(2)),
        ]));
        let meta = settings.metadata();
        assert_eq!(meta.len(), super::super::spec::NUMERIC_SPECS.len());
        let concurrency = meta
            .iter()
            .find(|m| m.key == "image_account_concurrency")
            .expect("应存在");
        assert_eq!(concurrency.current, 2.0);
        assert_eq!(concurrency.max, Some(3.0));
    }

    #[test]
    fn snapshot_is_isolated_from_later_writes() {
        let settings = RuntimeSettings::new(map(vec![("image_poll_timeout_secs", json!(60))]));
        let snap = settings.snapshot();
        settings.replace(map(vec![("image_poll_timeout_secs", json!(90))]), None);
        // 旧快照不受影响
        assert_eq!(snap.get("image_poll_timeout_secs"), Some(&json!(60)));
    }
}

//! 运行层设置的**契约表**——所有数值边界的唯一事实源。
//!
//! # 为什么集中一张表
//!
//! 边界如果散落在几十个 getter 里（各自调用 `clamp(1, 60)` 之类），
//! 改一个默认值要翻遍全项目、容易漏。这里集中成常量表，同时驱动：
//!
//! 1. **校验**（读/写时 clamp）
//! 2. **管理后台表单元数据**（默认值 / 上下限 / 单位 / 是否需重启）
//! 3. **文档**（一份表就是一份说明书）

/// 数值类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericKind {
    /// 整数。
    Integer,
    /// 浮点。
    Float,
}

/// 一项数值设置的规格。
#[derive(Debug, Clone, Copy)]
pub struct NumericSpec {
    /// 配置键名。
    pub key: &'static str,
    /// 默认值。
    pub default: f64,
    /// 最小值（含）。
    pub min: f64,
    /// 最大值（含），`None` 表示无上限。
    pub max: Option<f64>,
    /// 数值类型。
    pub kind: NumericKind,
    /// 单位（用于前端展示）。
    pub unit: Option<&'static str>,
    /// 修改后是否需要重启才生效。
    pub restart_required: bool,
    /// 人类可读说明。
    pub description: &'static str,
}

/// 运行层数值设置。
///
/// 数值与上游实测边界逐项对齐。
pub static NUMERIC_SPECS: &[NumericSpec] = &[
    NumericSpec {
        key: "image_input_max_mb",
        default: 10.0,
        min: 1.0,
        max: Some(50.0),
        kind: NumericKind::Integer,
        unit: Some("MB"),
        restart_required: false,
        description: "单张参考图（含 mask）的大小上限；超过直接拒绝，不读进内存。",
    },
    NumericSpec {
        key: "list_page_size",
        default: 50.0,
        min: 0.0,
        max: Some(1000.0),
        kind: NumericKind::Integer,
        unit: Some("条"),
        restart_required: false,
        description: "后台列表每页条数：50 / 100 常用，0 表示不分页（最多 1000 条）。",
    },
    NumericSpec {
        key: "image_archive_threshold_mb",
        default: 0.0,
        min: 0.0,
        max: Some(1_048_576.0),
        kind: NumericKind::Integer,
        unit: Some("MB"),
        restart_required: false,
        description: "在线图片总量超过该值自动归档最老的一批（0 = 关闭，默认关）。",
    },
    NumericSpec {
        key: "image_max_n",
        default: 4.0,
        min: 1.0,
        max: Some(32.0),
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "单次请求允许的最大张数；上游上限变化时改这里，不用发版。",
    },
    NumericSpec {
        key: "proxy_enabled",
        default: 0.0,
        min: 0.0,
        max: Some(1.0),
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "是否启用订阅节点作为全局出口：1 启用，0 直连。",
    },
    NumericSpec {
        key: "proxy_auto_refresh_minutes",
        default: 60.0,
        min: 5.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("minutes"),
        restart_required: false,
        description: "订阅自动刷新间隔；0 表示不自动刷新。",
    },
    NumericSpec {
        key: "proxy_probe_timeout_ms",
        default: 5000.0,
        min: 1000.0,
        max: Some(30000.0),
        kind: NumericKind::Integer,
        unit: Some("ms"),
        restart_required: false,
        description: "节点测速的单项超时。",
    },
    NumericSpec {
        key: "image_poll_timeout_secs",
        default: 60.0,
        min: 1.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("seconds"),
        restart_required: false,
        description: "图片结果轮询的总超时",
    },
    NumericSpec {
        key: "image_stream_timeout_secs",
        default: 80.0,
        min: 1.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("seconds"),
        restart_required: false,
        description: "SSE 阶段的上限；调大后单次请求的总时限同步变大",
    },
    NumericSpec {
        key: "image_poll_initial_wait_secs",
        default: 5.0,
        min: 0.0,
        max: Some(60.0),
        kind: NumericKind::Float,
        unit: Some("seconds"),
        restart_required: false,
        description: "开始轮询前的初始等待",
    },
    NumericSpec {
        key: "image_poll_interval_secs",
        default: 5.0,
        min: 0.5,
        max: Some(60.0),
        kind: NumericKind::Float,
        unit: Some("seconds"),
        restart_required: false,
        description: "轮询间隔",
    },
    NumericSpec {
        key: "image_account_concurrency",
        default: 1.0,
        min: 1.0,
        max: Some(3.0),
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "单账号并发上限",
    },
    NumericSpec {
        key: "account_processing_concurrency",
        default: 30.0,
        min: 1.0,
        max: Some(100.0),
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "账号批处理并发",
    },
    NumericSpec {
        key: "image_max_account_attempts",
        default: 4.0,
        min: 2.0,
        max: None,
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "单次请求最多尝试几个账号",
    },
    NumericSpec {
        key: "text_max_account_attempts",
        default: 3.0,
        min: 2.0,
        max: None,
        kind: NumericKind::Integer,
        unit: None,
        restart_required: false,
        description: "文本请求最多尝试几个账号",
    },
    NumericSpec {
        key: "image_settle_secs",
        default: 5.0,
        min: 0.5,
        max: None,
        kind: NumericKind::Float,
        unit: Some("seconds"),
        restart_required: false,
        description: "图片稳定判定等待",
    },
    NumericSpec {
        key: "image_retention_hours",
        default: 24.0,
        min: 1.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("hours"),
        restart_required: false,
        description: "图片保留时长",
    },
    NumericSpec {
        key: "log_retention_hours",
        default: 24.0,
        min: 1.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("hours"),
        restart_required: false,
        description: "日志保留时长",
    },
    NumericSpec {
        key: "console_request_timeout_secs",
        default: 600.0,
        min: 30.0,
        max: Some(3600.0),
        kind: NumericKind::Integer,
        unit: Some("seconds"),
        restart_required: false,
        description: "管理后台请求超时",
    },
    NumericSpec {
        key: "refresh_account_interval_minute",
        default: 5.0,
        min: 1.0,
        max: None,
        kind: NumericKind::Integer,
        unit: Some("minutes"),
        restart_required: false,
        description: "账号刷新间隔",
    },
    NumericSpec {
        key: "chat_cache_ttl_secs",
        default: 300.0,
        min: 0.0,
        max: Some(86400.0),
        kind: NumericKind::Integer,
        unit: Some("seconds"),
        restart_required: false,
        description: "文本结果缓存有效期；0 = 关闭",
    },
    NumericSpec {
        key: "image_task_concurrency",
        default: 2.0,
        min: 1.0,
        max: Some(16.0),
        kind: NumericKind::Integer,
        unit: None,
        restart_required: true,
        description: "异步生图任务并发上限（重启生效）",
    },
];

/// 按键查规格。
pub fn numeric_spec(key: &str) -> Option<&'static NumericSpec> {
    NUMERIC_SPECS.iter().find(|spec| spec.key == key)
}

/// 按契约语义归一化一个数值。
///
/// ```text
/// 输入非法（None / NaN / Inf）   default
/// 小于 min                        min
/// 大于 max                        max（max 为 None 则不设上限）
/// 不报错，只钳制
/// ```
///
/// > 与 Python 的唯一差异：Python 里 `float('nan')` 能成功转换，
/// > 后续 `max/min` 比较行为依赖实现细节。这里明确把 NaN/Inf 当作非法值回退默认，
/// > 行为可预测。
pub fn normalize_f64(spec: &NumericSpec, raw: Option<f64>) -> f64 {
    let mut value = match raw {
        Some(v) if v.is_finite() => v,
        _ => spec.default,
    };
    if value < spec.min {
        value = spec.min;
    }
    if let Some(max) = spec.max {
        if value > max {
            value = max;
        }
    }
    value
}

/// 归一化成整数（先钳制再截断）。
pub fn normalize_i64(spec: &NumericSpec, raw: Option<f64>) -> i64 {
    normalize_f64(spec, raw).trunc() as i64
}

/// 一项字符串设置的规格。
#[derive(Debug, Clone, Copy)]
pub struct StringSpec {
    /// 配置键名。
    pub key: &'static str,
    /// 默认值。
    pub default: &'static str,
    /// 最大长度（字符数），超出则截断。
    pub max_len: usize,
    /// 修改后是否需要重启才生效。
    pub restart_required: bool,
    /// 人类可读说明。
    pub description: &'static str,
}

/// 字符串类设置。
///
/// 与数值项分开列，因为它们的校验方式不同（长度上限 vs 区间钳制）。
/// **新增字符串设置时必须加在这里**，否则 `/api/settings` 会以「未知键」拒收 ——
/// 这正是 `chat_blocked_keywords` 曾经踩过的坑：代码读了它，但契约里没有，
/// 于是没有任何途径能把它设进去，功能形同虚设。
pub static STRING_SPECS: &[StringSpec] = &[
    StringSpec {
        key: "proxy_subscription_url",
        default: "",
        max_len: 1024,
        restart_required: false,
        description: "订阅地址（Clash 格式）。保存后由管理面「开始」按钮拉取并启动代理核。",
    },
    StringSpec {
        key: "proxy_selected_node",
        default: "",
        max_len: 128,
        restart_required: false,
        description: "当前选中的出口节点名，重启后恢复。",
    },
    StringSpec {
        key: "proxy_probe_url",
        default: "https://www.gstatic.com/generate_204",
        max_len: 256,
        restart_required: false,
        description: "测速探测目标地址。",
    },
    StringSpec {
        key: "rich_text_policy",
        default: "guard+strip",
        max_len: 32,
        restart_required: false,
        description: "富文本标签处理策略：off / guard / strip / guard+strip。\n\
                      guard = 提示模型只输出纯文本；strip = 剥掉自闭合富文本组件。",
    },
    StringSpec {
        key: "chat_blocked_keywords",
    default: "",
    max_len: 2000,
    restart_required: false,
    description: "文本对话的内容粗筛关键词（逗号分隔）。空 = 不拦截。\n\
                  注意：这是**子串匹配，不是分类器**，只用于挡住最明显的滥用。",
    },
];

/// 按 key 查字符串规格。
pub fn string_spec(key: &str) -> Option<&'static StringSpec> {
    STRING_SPECS.iter().find(|spec| spec.key == key)
}

/// 归一化字符串设置：去首尾空白 + 按 `max_len` 截断。
///
/// 截断而不是报错：前端多打几个字符不该让整个保存失败。
/// 但**长度上限必须存在**，否则一个超长串会被写进设置、然后每个请求都重复解析。
pub fn normalize_string(spec: &StringSpec, value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= spec.max_len {
        return trimmed.to_string();
    }
    trimmed.chars().take(spec.max_len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(key: &str) -> &'static NumericSpec {
        numeric_spec(key).unwrap_or_else(|| panic!("未找到规格: {key}"))
    }

    #[test]
    fn all_expected_keys_present() {
        let expected = [
            "image_poll_timeout_secs",
            "image_stream_timeout_secs",
            "image_poll_initial_wait_secs",
            "image_poll_interval_secs",
            "image_account_concurrency",
            "account_processing_concurrency",
            "image_max_account_attempts",
            "text_max_account_attempts",
            "image_settle_secs",
            "image_retention_hours",
            "log_retention_hours",
            "console_request_timeout_secs",
            "refresh_account_interval_minute",
            "chat_cache_ttl_secs",
            "image_task_concurrency",
            "proxy_enabled",
            "proxy_auto_refresh_minutes",
            "proxy_probe_timeout_ms",
            "image_input_max_mb",
            "image_archive_threshold_mb",
            "image_max_n",
            "list_page_size",
        ];
        assert_eq!(NUMERIC_SPECS.len(), expected.len());
        for key in expected {
            assert!(numeric_spec(key).is_some(), "缺少规格: {key}");
        }
    }

    #[test]
    fn key_is_unique() {
        for (index, spec) in NUMERIC_SPECS.iter().enumerate() {
            let dup = NUMERIC_SPECS
                .iter()
                .skip(index + 1)
                .any(|other| other.key == spec.key);
            assert!(!dup, "重复的 key: {}", spec.key);
        }
    }

    #[test]
    fn defaults_are_within_bounds() {
        for spec in NUMERIC_SPECS {
            assert!(
                spec.default >= spec.min,
                "{} 默认值 {} 小于下限 {}",
                spec.key,
                spec.default,
                spec.min
            );
            if let Some(max) = spec.max {
                assert!(
                    spec.default <= max,
                    "{} 默认值 {} 大于上限 {}",
                    spec.key,
                    spec.default,
                    max
                );
                assert!(spec.min <= max, "{} min > max", spec.key);
            }
        }
    }

    #[test]
    fn clamp_below_min() {
        let s = spec("image_account_concurrency"); // 1..=3
        assert_eq!(normalize_f64(s, Some(-100.0)), 1.0);
        assert_eq!(normalize_i64(s, Some(0.0)), 1);
    }

    #[test]
    fn clamp_above_max() {
        let s = spec("image_account_concurrency"); // 1..=3
        assert_eq!(normalize_f64(s, Some(999.0)), 3.0);
        assert_eq!(normalize_i64(s, Some(999.0)), 3);
    }

    #[test]
    fn no_max_means_unbounded() {
        let s = spec("image_poll_timeout_secs"); // min 1, no max
        assert_eq!(normalize_f64(s, Some(1_000_000.0)), 1_000_000.0);
    }

    #[test]
    fn invalid_falls_back_to_default() {
        let s = spec("image_poll_interval_secs"); // default 5.0
        assert_eq!(normalize_f64(s, None), 5.0);
        assert_eq!(normalize_f64(s, Some(f64::NAN)), 5.0);
        assert_eq!(normalize_f64(s, Some(f64::INFINITY)), 5.0);
        assert_eq!(normalize_f64(s, Some(f64::NEG_INFINITY)), 5.0);
    }

    #[test]
    fn float_kind_keeps_fraction() {
        let s = spec("image_poll_interval_secs"); // 0.5..=60 float
        assert_eq!(normalize_f64(s, Some(0.5)), 0.5);
        assert_eq!(normalize_f64(s, Some(12.75)), 12.75);
    }

    #[test]
    fn int_kind_truncates() {
        let s = spec("image_poll_timeout_secs");
        assert_eq!(normalize_i64(s, Some(59.9)), 59);
        assert_eq!(normalize_i64(s, Some(60.1)), 60);
    }

    #[test]
    fn boundary_values_are_inclusive() {
        let s = spec("console_request_timeout_secs"); // 30..=3600
        assert_eq!(normalize_i64(s, Some(30.0)), 30);
        assert_eq!(normalize_i64(s, Some(3600.0)), 3600);
        assert_eq!(normalize_i64(s, Some(29.9)), 30);
        assert_eq!(normalize_i64(s, Some(3600.1)), 3600);
    }
}


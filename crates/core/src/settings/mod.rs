//! 运行层设置：契约表 + 内存快照。
//!
//! [`spec`] 是数值与字符串边界的唯一事实源；[`runtime`] 持有快照并按契约归一化。

pub mod runtime;
pub mod spec;

pub use runtime::{FieldMeta, RuntimeSettings};
pub use spec::{
    NUMERIC_SPECS, NumericKind, NumericSpec, STRING_SPECS, StringSpec, normalize_f64,
    normalize_i64, normalize_string, numeric_spec, string_spec,
};

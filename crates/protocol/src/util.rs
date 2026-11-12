//! protocol crate 内部小工具。

/// 生成 uuid v4 字符串。
///
/// 单独包一层，避免让上层模块直接依赖 `uuid` crate。
pub(crate) fn util_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 生成 uuid v4 字符串（对外）。
pub fn new_uuid() -> String {
    util_uuid()
}

/// 当前 Unix 秒（带小数）。
pub fn now_unix_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn unix_secs_plausible() {
        let now = now_unix_secs_f64();
        assert!(now > 1_704_067_200.0);
    }
}

//! 模型清单与「对外模型名  上游 slug」映射。

/// 上游默认模型 slug。
pub use gpt2apirs_protocol::conversation::DEFAULT_MODEL;

/// 当前**真正支持**的对外模型名。
pub const SUPPORTED_MODELS: &[&str] = &["gpt-image-2"];

/// 上游还有、但我们尚未实现的模型（仅用于文档与错误提示）。
pub const NOT_YET_SUPPORTED_MODELS: &[&str] = &[
    "codex-gpt-image-2",
    "plus-codex-gpt-image-2",
    "team-codex-gpt-image-2",
    "pro-codex-gpt-image-2",
];

/// 默认对外模型名。
pub fn default_model() -> &'static str {
    SUPPORTED_MODELS[0]
}

/// 该对外模型名是否受支持。
pub fn is_supported(model: &str) -> bool {
    let trimmed = model.trim();
    trimmed.is_empty() || SUPPORTED_MODELS.iter().any(|m| *m == trimmed)
}

/// 把对外模型名映射成上游 slug。
///
/// 当前所有受支持的模型都走同一条 web 生图链路。
pub fn upstream_model_for(model: &str) -> &'static str {
    let _ = model;
    DEFAULT_MODEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_supported() {
        assert!(is_supported(default_model()));
    }

    #[test]
    fn empty_model_is_treated_as_default() {
        assert!(is_supported(""));
        assert!(is_supported("   "));
    }

    #[test]
    fn codex_models_are_not_yet_supported() {
        for model in NOT_YET_SUPPORTED_MODELS {
            assert!(!is_supported(model), "{model} 不应被标记为支持");
        }
    }

    #[test]
    fn unknown_model_rejected() {
        assert!(!is_supported("dall-e-3"));
        assert!(!is_supported("gpt-4o"));
    }

    #[test]
    fn upstream_mapping_is_stable() {
        assert_eq!(upstream_model_for("gpt-image-2"), DEFAULT_MODEL);
        assert_eq!(upstream_model_for(""), DEFAULT_MODEL);
    }
}

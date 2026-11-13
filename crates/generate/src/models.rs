//! 模型清单与「对外模型名 → 上游 slug」映射。
//!
//! 图片模型与文本模型是**两套清单**：图片路由用 `SUPPORTED_MODELS` 校验，
//! 文本路由只认 `TEXT_MODELS` 里的默认名，其余名字原样透传。
//! `/v1/models` 以 `MODEL_CATALOG` 为准 —— 新增模型只改这里，不要在路由里硬编码。

/// 上游默认模型 slug。
pub use gpt2apirs_protocol::conversation::DEFAULT_MODEL;

/// 唯一支持的图片模型对外名。
pub const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";

/// 当前**真正支持**的图片模型对外名。
pub const SUPPORTED_MODELS: &[&str] = &[DEFAULT_IMAGE_MODEL];

/// 上游还有、但我们尚未实现的模型（仅用于文档与错误提示）。
pub const NOT_YET_SUPPORTED_MODELS: &[&str] = &[
    "codex-gpt-image-2",
    "plus-codex-gpt-image-2",
    "team-codex-gpt-image-2",
    "pro-codex-gpt-image-2",
];

/// 默认文本模型对外名。
pub const DEFAULT_TEXT_MODEL: &str = "gpt-5-5";

/// 文本模型的对外名。
///
/// 上游只有一条文本链路，这些名字只是对外标签；不在表里的名字照样透传，
/// 所以这里只列**我们愿意背书**的名字。
pub const TEXT_MODELS: &[&str] = &[DEFAULT_TEXT_MODEL];

/// 模型用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// 文本对话。
    Text,
    /// 图片生成 / 编辑。
    Image,
}

impl ModelKind {
    /// 对外的用途标签。
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Text => "text",
            ModelKind::Image => "image",
        }
    }
}

/// 一条模型目录项。
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    /// 对外模型名。
    pub id: &'static str,
    /// 用途。
    pub kind: ModelKind,
}

/// 对外模型目录：`/v1/models` 与 `GET /v1/models/{id}` 的唯一来源。
pub const MODEL_CATALOG: &[ModelInfo] = &[
    ModelInfo {
        id: DEFAULT_IMAGE_MODEL,
        kind: ModelKind::Image,
    },
    ModelInfo {
        id: DEFAULT_TEXT_MODEL,
        kind: ModelKind::Text,
    },
];

/// 默认图片模型对外名。
pub fn default_image_model() -> &'static str {
    DEFAULT_IMAGE_MODEL
}

/// 该名字是否是受支持的**图片**模型。
///
/// 空名字视为使用默认模型；文本模型名不算通过 —— 否则
/// `/v1/images/generations` 会接受 `gpt-5-5`。
pub fn is_supported_image(model: &str) -> bool {
    let trimmed = model.trim();
    trimmed.is_empty() || SUPPORTED_MODELS.iter().any(|m| *m == trimmed)
}

/// 按对外名查目录项。
pub fn find(model: &str) -> Option<&'static ModelInfo> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    MODEL_CATALOG.iter().find(|info| info.id == trimmed)
}

/// 把对外模型名映射成上游 slug。
///
/// 当前所有模型（文本与图片）都走同一条上游会话链路。
pub fn upstream_model_for(model: &str) -> &'static str {
    let _ = model;
    DEFAULT_MODEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_declared_model() {
        for id in SUPPORTED_MODELS {
            let info = find(id).unwrap_or_else(|| panic!("{id} 不在目录里"));
            assert_eq!(info.kind, ModelKind::Image, "{id}");
        }
        for id in TEXT_MODELS {
            let info = find(id).unwrap_or_else(|| panic!("{id} 不在目录里"));
            assert_eq!(info.kind, ModelKind::Text, "{id}");
        }
        assert_eq!(MODEL_CATALOG.len(), SUPPORTED_MODELS.len() + TEXT_MODELS.len());
    }

    #[test]
    fn default_image_model_is_supported() {
        assert!(is_supported_image(default_image_model()));
    }

    #[test]
    fn empty_model_is_treated_as_default() {
        assert!(is_supported_image(""));
        assert!(is_supported_image("   "));
    }

    #[test]
    fn text_model_is_not_an_image_model() {
        assert!(!is_supported_image(DEFAULT_TEXT_MODEL));
        assert!(!is_supported_image("gpt-5-5"));
    }

    #[test]
    fn codex_models_are_not_yet_supported() {
        for model in NOT_YET_SUPPORTED_MODELS {
            assert!(!is_supported_image(model), "{model} 不应被标记为支持");
            assert!(find(model).is_none(), "{model} 不应出现在目录里");
        }
    }

    #[test]
    fn unknown_model_rejected() {
        assert!(!is_supported_image("dall-e-3"));
        assert!(!is_supported_image("gpt-4o"));
        assert!(find("dall-e-3").is_none());
    }

    #[test]
    fn lookup_is_trimmed_and_typed() {
        let text = find(" gpt-5-5 ").expect("应找到文本模型");
        assert_eq!(text.kind, ModelKind::Text);
        assert_eq!(find("gpt-image-2").map(|i| i.kind), Some(ModelKind::Image));
        assert!(find("").is_none(), "空名字不该命中目录");
    }

    #[test]
    fn upstream_mapping_is_stable() {
        assert_eq!(upstream_model_for("gpt-image-2"), DEFAULT_MODEL);
        assert_eq!(upstream_model_for(""), DEFAULT_MODEL);
    }
}

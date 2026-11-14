//! 图片端点共用的小工具。
//!
//! 只放「三个端点必须一致」的校验，避免同一个参数在不同端点上行为不同。

use gpt2apirs_core::AppError;

use crate::error::ApiError;

/// 校验并规范化 `size`。
///
/// `auto`/空 → `None`；合法 `宽x高` → 规范化字符串；其余 400（`param = size`）。
/// 注意：返回值只是**提示词里的倾向**，上游不保证按它出图。
pub fn checked_size(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(text) = raw else { return Ok(None) };
    let normalized = gpt2apirs_generate::format::parse_size(text)
        .map_err(|message| ApiError::openai(AppError::invalid_param(message, "size")))?;
    Ok(normalized)
}

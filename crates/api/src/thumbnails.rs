//! 缩略图：懒生成 + 磁盘缓存。
//!
//! 放在 `data/thumbnails/` 下，**不进归档包**，归档或删图时一并删除——
//! 它是可丢弃的派生数据，随时能按原图重建。

use std::path::{Path, PathBuf};

use anyhow::Context;

/// 长边上限（像素）。
pub const MAX_EDGE: u32 = 480;
/// WebP 质量（0-100）。
const QUALITY: f32 = 80.0;

/// 缩略图缓存目录。
pub fn dir() -> anyhow::Result<PathBuf> {
    Ok(gpt2apirs_core::paths::data_dir()?.join("thumbnails"))
}

/// 相对路径 → 缩略图相对路径；非法路径返回 `None`。
pub fn thumb_rel(rel_path: &str) -> Option<String> {
    let normalized = rel_path.trim().replace('\\', "/");
    if normalized.is_empty() || normalized.contains("..") || !normalized.starts_with("images/") {
        return None;
    }
    let stem = Path::new(&normalized).with_extension("webp");
    Some(format!("thumbnails/{}", stem.display()))
}

/// 取缩略图，必要时生成。原图不存在时返回 `Ok(None)`。
pub fn ensure(rel_path: &str) -> anyhow::Result<Option<PathBuf>> {
    let root = gpt2apirs_core::paths::data_dir()?;
    ensure_in(&root, rel_path)
}

/// 同上，但数据根目录由调用方给出（测试与批处理用）。
pub fn ensure_in(root: &Path, rel_path: &str) -> anyhow::Result<Option<PathBuf>> {
    let Some(thumb_rel) = thumb_rel(rel_path) else {
        return Ok(None);
    };
    let thumb = root.join(thumb_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    if thumb.is_file() {
        return Ok(Some(thumb));
    }
    let source = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if !source.is_file() {
        return Ok(None);
    }
    if let Some(parent) = thumb.parent() {
        std::fs::create_dir_all(parent).context("建缩略图目录失败")?;
    }
    let bytes = std::fs::read(&source).context("读原图失败")?;
    let image = image::load_from_memory(&bytes).context("解码原图失败")?;
    let scaled = image.thumbnail(MAX_EDGE, MAX_EDGE);
    // WebP 支持透明通道，直接按 RGBA 编码，不用铺底色。
    let rgba = scaled.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let encoded = webp::Encoder::from_rgba(rgba.as_raw(), width, height).encode(QUALITY);
    let temp = thumb.with_extension("webp.part");
    std::fs::write(&temp, &*encoded).context("写缩略图失败")?;
    std::fs::rename(&temp, &thumb).context("重命名缩略图失败")?;
    Ok(Some(thumb))
}

/// 删除某张图的缩略图缓存（不存在视为已删）。
pub fn remove(rel_path: &str) {
    let Ok(root) = gpt2apirs_core::paths::data_dir() else {
        return;
    };
    remove_in(&root, rel_path);
}

/// 同上，数据根目录由调用方给出。
pub fn remove_in(root: &Path, rel_path: &str) {
    let Some(thumb_rel) = thumb_rel(rel_path) else {
        return;
    };
    let path = root.join(thumb_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, path = %path.display(), "删除缩略图失败"),
    }
}

/// 校验缩略图请求路径是否指向 `images/` 下的允许格式。
pub fn is_allowed_source(rel_path: &str) -> bool {
    let Some(thumb_rel) = thumb_rel(rel_path) else {
        return false;
    };
    if thumb_rel.contains("..") {
        return false;
    }
    matches!(
        Path::new(rel_path).extension().and_then(|value| value.to_str()),
        Some("png" | "jpg" | "jpeg" | "webp" | "gif")
    )
}

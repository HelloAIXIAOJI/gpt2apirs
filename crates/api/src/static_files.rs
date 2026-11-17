//! 静态图片服务：`GET /images/{path}`。
//!
//!
//! # 安全
//!
//! 必须防**路径穿越**（`../../etc/passwd`）。做法：
//!
//! 1. 词法规范化路径
//! 2. 拒绝任何 `..` 残留
//! 3. 拼接后再校验结果仍以 images 目录开头

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gpt2apirs_core::AppError;

use crate::error::ApiError;
use crate::state::AppState;

/// `GET /images/{*path}`
pub async fn serve_image(
    State(_state): State<Arc<AppState>>,
    AxumPath(relative): AxumPath<String>,
) -> Response {
    match resolve_path(&relative) {
        Ok(path) => match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mime = mime_of(&path);
                ([(header::CONTENT_TYPE, mime)], bytes).into_response()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                ApiError::openai(AppError::not_found("图片不存在"))
                .into_response()
            }
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "读取图片失败");
                ApiError::openai(AppError::Internal(anyhow::anyhow!("读取图片失败")))
                    .into_response()
            }
        },
        Err(err) => err.into_response(),
    }
}

fn resolve_path(relative: &str) -> Result<PathBuf, ApiError> {
    let cleaned = relative.trim_start_matches('/');
    if cleaned.is_empty() {
        return Err(ApiError::openai(AppError::not_found("图片不存在")));
    }
    // 拒绝路径穿越
    if cleaned.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(ApiError::openai(AppError::Forbidden {
            message: "非法路径".to_string(),
        }));
    }

    let images_root = gpt2apirs_core::paths::images_dir().map_err(|err| {
        ApiError::openai(AppError::Internal(anyhow::anyhow!(err.to_string())))
    })?;
    let candidate = images_root.join(cleaned.replace('/', std::path::MAIN_SEPARATOR_STR));

    // 双保险：规范化后必须仍在 images 根目录内
    let normalized = gpt2apirs_core::paths::normalize(&candidate);
    let root_normalized = gpt2apirs_core::paths::normalize(&images_root);
    if !normalized.starts_with(&root_normalized) {
        return Err(ApiError::openai(AppError::Forbidden {
            message: "非法路径".to_string(),
        }));
    }
    Ok(normalized)
}

fn mime_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        for bad in ["../etc/passwd", "a/../../b", "..\\windows", "x/../y"] {
            assert!(
                resolve_path(bad).is_err(),
                "应拒绝路径穿越: {bad}"
            );
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(resolve_path("").is_err());
        assert!(resolve_path("/").is_err());
    }

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_of(Path::new("a.png")), "image/png");
        assert_eq!(mime_of(Path::new("a.JPG")), "image/jpeg");
        assert_eq!(mime_of(Path::new("a.webp")), "image/webp");
        assert_eq!(mime_of(Path::new("a.bin")), "application/octet-stream");
    }
}

/// `GET /image-thumbnails/{*path}` —— 懒生成缩略图并直接返回。
///
/// 缩略图是可丢弃的派生数据：缓存放在 `data/thumbnails`，不进归档包，
/// 归档或删图时会一并删除。
pub async fn serve_thumbnail(AxumPath(path): AxumPath<String>) -> Response {
    let rel_path = format!("images/{}", path.trim_start_matches('/'));
    if !crate::thumbnails::is_allowed_source(&rel_path) {
        return (StatusCode::BAD_REQUEST, "路径不合法").into_response();
    }
    let thumb = match tokio::task::spawn_blocking({
        let rel_path = rel_path.clone();
        move || crate::thumbnails::ensure(&rel_path)
    })
    .await
    {
        Ok(Ok(Some(path))) => path,
        Ok(Ok(None)) => return (StatusCode::NOT_FOUND, "图片不存在").into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, path = %rel_path, "生成缩略图失败");
            return (StatusCode::INTERNAL_SERVER_ERROR, "生成缩略图失败").into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "缩略图任务失败");
            return (StatusCode::INTERNAL_SERVER_ERROR, "生成缩略图失败").into_response();
        }
    };
    match tokio::fs::read(&thumb).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp".to_string()),
                (header::CACHE_CONTROL, "private, max-age=600".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "缩略图不存在").into_response(),
    }
}

//! 上游图片上传。
//!
//! 图生图必须先上传参考图，上游用 Azure Blob 做存储，**三步**：
//!
//! 三步上传，缺一步就白传：`POST /backend-api/files` 申请上传位拿 `file_id` 与 `upload_url`；
//! `PUT {upload_url}` 把原始字节传到 Blob（预签名 URL，**不要带 Authorization**）；
//! `POST /backend-api/files/{file_id}/uploaded` 确认，否则会话里引用不到这个文件。
//!
//! 少任何一步，会话 payload 里的 `file-service://{file_id}` 都会指空。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use gpt2apirs_protocol::endpoints::{BASE_URL, FILE_UPLOAD, file_uploaded};
use serde_json::Value;

use crate::image_meta;

/// 上传成功后的文件引用。
#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// 上游文件 ID，会话 payload 里用 `file-service://{file_id}` 引用。
    pub file_id: String,
    /// 文件名。
    pub file_name: String,
    /// 字节数。
    pub file_size: usize,
    /// MIME。
    pub mime_type: String,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

impl UploadedFile {
    /// 转成会话 payload 用的引用结构。
    pub fn as_reference(&self) -> gpt2apirs_protocol::conversation::ImageReference {
        gpt2apirs_protocol::conversation::ImageReference {
            file_id: self.file_id.clone(),
            file_name: self.file_name.clone(),
            mime_type: self.mime_type.clone(),
            size_bytes: self.file_size,
            width: self.width,
            height: self.height,
        }
    }
}

/// 各步骤的超时上限。
mod caps {
    use std::time::Duration;
    /// 申请上传位。
    pub const CREATE: Duration = Duration::from_secs(30);
    /// 上传字节（大图慢）。
    pub const PUT: Duration = Duration::from_secs(120);
    /// 确认。
    pub const CONFIRM: Duration = Duration::from_secs(30);
}

fn budget(deadline: Deadline, stage: &'static str, cap: Duration) -> Result<Duration> {
    deadline
        .budget(stage, cap)
        .map_err(|_| anyhow::anyhow!("请求总时限已耗尽（阶段 {stage}）"))
}

/// 上传一张图片。
pub async fn upload_image(
    client: &Client,
    token: &str,
    user_agent: &str,
    bytes: &[u8],
    file_name_hint: Option<&str>,
    deadline: Deadline,
) -> Result<UploadedFile> {
    if bytes.is_empty() {
        bail!("图片内容为空");
    }
    let meta = image_meta(bytes)
        .context("不是受支持的图片格式（PNG / JPEG / WebP / GIF）")?;
    let file_name = file_name_hint
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("image.{}", meta.ext));

    // ---- 1) 申请上传位 ----
    let create_timeout = budget(deadline, "upload_create", caps::CREATE)?;
    let path = FILE_UPLOAD;
    let body = serde_json::json!({
        "file_name": file_name,
        "file_size": bytes.len(),
        "use_case": "multimodal",
        "width": meta.width,
        "height": meta.height,
    });
    let response = client
        .post_json(
            &format!("{BASE_URL}{path}"),
            &gpt2apirs_http::api_headers(user_agent, token, path),
            &body,
            Some(create_timeout),
        )
        .await
        .context("申请上传位失败")?;
    if response.status != 200 {
        bail!(
            "申请上传位返回 {}：{}",
            response.status,
            clip(&response.body)
        );
    }
    let created: Value =
        serde_json::from_str(&response.body).context("上传位响应不是合法 JSON")?;
    let file_id = created
        .get("file_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .context("上传位响应里没有 file_id")?;
    let upload_url = created
        .get("upload_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .context("上传位响应里没有 upload_url")?;

    // ---- 2) PUT 原始字节到 Blob ----
    // 预签名 URL 自带鉴权，带上 Authorization 反而会被拒。
    let put_timeout = budget(deadline, "upload_put", caps::PUT)?;
    let put_headers = vec![
        ("Content-Type", meta.mime.to_string()),
        ("x-ms-blob-type", "BlockBlob".to_string()),
        ("x-ms-version", "2020-04-08".to_string()),
        ("Origin", BASE_URL.to_string()),
        ("Referer", format!("{BASE_URL}/")),
        ("User-Agent", user_agent.to_string()),
        (
            "Accept",
            "application/json, text/plain, */*".to_string(),
        ),
        ("Accept-Language", "en-US,en;q=0.8".to_string()),
    ];
    let put = client
        .put_bytes(&upload_url, &put_headers, bytes.to_vec(), Some(put_timeout))
        .await
        .context("上传字节失败")?;
    if !(200..300).contains(&put.status) {
        bail!("上传字节返回 {}：{}", put.status, clip(&put.body));
    }

    // ---- 3) 确认上传完成 ----
    let confirm_timeout = budget(deadline, "upload_confirm", caps::CONFIRM)?;
    let confirm_path = file_uploaded(&file_id);
    let confirm = client
        .post_json(
            &format!("{BASE_URL}{confirm_path}"),
            &gpt2apirs_http::api_headers(user_agent, token, &confirm_path),
            &serde_json::json!({}),
            Some(confirm_timeout),
        )
        .await
        .context("确认上传失败")?;
    if confirm.status != 200 {
        bail!(
            "确认上传返回 {}：{}",
            confirm.status,
            clip(&confirm.body)
        );
    }

    Ok(UploadedFile {
        file_id,
        file_name,
        file_size: bytes.len(),
        mime_type: meta.mime.to_string(),
        width: meta.width,
        height: meta.height,
    })
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let clipped: String = text.chars().take(LIMIT).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("编码 PNG");
        buffer.into_inner()
    }

    #[test]
    fn image_meta_reads_real_dimensions() {
        let bytes = png_bytes(37, 19);
        let meta = image_meta(&bytes).expect("应能解析");
        assert_eq!(meta.mime, "image/png");
        assert_eq!(meta.ext, "png");
        assert_eq!(meta.width, 37);
        assert_eq!(meta.height, 19);
    }

    #[test]
    fn image_meta_rejects_non_image() {
        assert!(image_meta(b"not an image at all").is_none());
    }

    #[tokio::test]
    async fn upload_rejects_empty_bytes() {
        // 空字节应在发请求前就被拒（不依赖网络）
        let client = Client::new(Default::default()).expect("客户端");
        let error = upload_image(&client, "tok", "ua", b"", None, Deadline::in_secs(5.0))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("为空"), "{error}");
    }

    #[tokio::test]
    async fn upload_rejects_non_image_bytes() {
        let client = Client::new(Default::default()).expect("客户端");
        let error = upload_image(
            &client,
            "tok",
            "ua",
            b"definitely not an image",
            None,
            Deadline::in_secs(5.0),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("图片格式"), "{error}");
    }
}

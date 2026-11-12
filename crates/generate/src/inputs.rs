//! 图片输入解析：把「用户给的图片引用」统一成字节。
//!
//! 图生图的图片来源在真实使用里非常杂，必须全都接住：
//!
//! 上游只认字节，所以这一步必须把上面全部归一。

use std::time::Duration;

use base64::Engine as _;
use gpt2apirs_core::Deadline;
use gpt2apirs_http::Client;
use serde_json::Value;

use crate::sniff_mime;

/// 单张图片的体积上限（50 MB）。
pub const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;

/// 下载远程图片时使用的 User-Agent。
///
/// 刻意**不用** `DEFAULT_USER_AGENT`：这是以「我们的服务」身份去取一个第三方 URL，
/// 冒充浏览器没有必要，也不礼貌。
const FETCH_USER_AGENT: &str = "gpt2apirs image fetcher";

/// 一个尚未取到字节的图片来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// 已经是字节（multipart 上传 / 已经解好的 base64）。
    Inline {
        /// 字节。
        data: Vec<u8>,
        /// 文件名。
        filename: String,
        /// MIME。
        mime: String,
    },
    /// `data:...` 内联 URL，需要解码。
    DataUrl(String),
    /// `http(s)://` 远程 URL，需要下载。
    RemoteUrl(String),
}

/// 取到字节后的图片。
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    /// 字节。
    pub data: Vec<u8>,
    /// 文件名。
    pub filename: String,
    /// MIME（以嗅探结果为准，声明值只作兜底）。
    pub mime: String,
}

/// 解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// 没有提供任何图片。
    #[error("image file or image_url is required")]
    Missing,

    /// base64 非法。
    #[error("invalid base64 image data: {0}")]
    InvalidBase64(String),

    /// data URL 非法。
    #[error("invalid data image URL: {0}")]
    InvalidDataUrl(String),

    /// 超过体积上限。
    #[error("image exceeds 50MB limit (actual {size} bytes)")]
    TooLarge {
        /// 实际字节数。
        size: usize,
    },

    /// 空内容。
    #[error("image content is empty")]
    Empty,

    /// 仅支持 http/https。
    #[error("image_url must be an http or https URL")]
    NotHttpUrl,

    /// 下载失败。
    #[error("image_url fetch failed: {0}")]
    Fetch(String),

    /// 不是受支持的图片。
    #[error("not a supported image (PNG / JPEG / WebP / GIF)")]
    NotAnImage,

    /// 明确不支持 `file_id` 引用。
    #[error("file_id image references are not supported; use image_url instead")]
    FileIdUnsupported,
}

fn check_size(size: usize) -> Result<(), InputError> {
    if size == 0 {
        return Err(InputError::Empty);
    }
    if size > MAX_IMAGE_BYTES {
        return Err(InputError::TooLarge { size });
    }
    Ok(())
}

/// 解析一个文本形式的图片引用。
///
/// 判定顺序：空  `data:`  `http(s)://`  当作裸 base64。
pub fn sources_from_text(text: &str) -> Result<Vec<ImageSource>, InputError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("data:") {
        return Ok(vec![ImageSource::DataUrl(trimmed.to_string())]);
    }
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        return Ok(vec![ImageSource::RemoteUrl(trimmed.to_string())]);
    }
    // 剩下的按裸 base64 处理。粘贴来的 base64 常带换行或空格，先剔掉再解。
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let data = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|error| InputError::InvalidBase64(error.to_string()))?;
    check_size(data.len())?;
    Ok(vec![ImageSource::Inline {
        data,
        filename: "image.png".to_string(),
        mime: "image/png".to_string(),
    }])
}

/// 解析一个 JSON 图片引用（字符串 / 数组 / 对象）。
pub fn sources_from_json(value: &Value) -> Result<Vec<ImageSource>, InputError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => sources_from_text(text),
        Value::Array(items) => {
            let mut collected = Vec::new();
            for item in items {
                collected.extend(sources_from_json(item)?);
            }
            Ok(collected)
        }
        Value::Object(map) => source_from_object(map),
        other => Err(InputError::InvalidDataUrl(format!(
            "unsupported image reference: {other}"
        ))),
    }
}

fn source_from_object(
    map: &serde_json::Map<String, Value>,
) -> Result<Vec<ImageSource>, InputError> {
    // 明确拒绝 file_id：那是上游内部标识，外部无法构造出可用引用
    if map
        .get("file_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(InputError::FileIdUnsupported);
    }
    // 内联 base64
    for key in ["b64_json", "base64"] {
        if let Some(payload) = map.get(key).and_then(Value::as_str) {
            if !payload.trim().is_empty() {
                let filename =
                    pick(map, &["filename", "file_name"]).unwrap_or_else(|| "image.png".to_string());
                let mime = pick(map, &["mime_type", "mimeType"])
                    .unwrap_or_else(|| "image/png".to_string());
                let compact: String =
                    payload.chars().filter(|c| !c.is_whitespace()).collect();
                let data = base64::engine::general_purpose::STANDARD
                    .decode(compact.as_bytes())
                    .map_err(|error| InputError::InvalidBase64(error.to_string()))?;
                check_size(data.len())?;
                return Ok(vec![ImageSource::Inline {
                    data,
                    filename,
                    mime,
                }]);
            }
        }
    }
    // image_url / url（可能是字符串，也可能是 {url: "..."}）
    let mut raw = map.get("image_url").or_else(|| map.get("url"));
    if let Some(Value::Object(inner)) = raw {
        raw = inner.get("url");
    }
    match raw {
        Some(Value::String(text)) => sources_from_text(text),
        Some(other @ (Value::Array(_) | Value::Object(_))) => sources_from_json(other),
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(other) => Err(InputError::InvalidDataUrl(format!(
            "unsupported image_url: {other}"
        ))),
    }
}

fn pick(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = map.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// 解码 `data:` URL。
pub fn decode_data_url(url: &str) -> Result<ResolvedImage, InputError> {
    let (header, payload) = url
        .split_once(',')
        .ok_or_else(|| InputError::InvalidDataUrl("缺少逗号分隔符".to_string()))?;
    let mime = header
        .trim_start_matches("data:")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let mime = if mime.is_empty() {
        "image/png".to_string()
    } else {
        mime
    };
    if !mime.starts_with("image/") {
        return Err(InputError::InvalidDataUrl(format!(
            "不是图片类型：{mime}"
        )));
    }
    let is_base64 = header.to_ascii_lowercase().contains(";base64");
    let data = if is_base64 {
        let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact.as_bytes())
            .map_err(|error| InputError::InvalidDataUrl(error.to_string()))?
    } else {
        percent_decode(payload)
    };
    check_size(data.len())?;
    let ext = extension_for_mime(&mime);
    Ok(ResolvedImage {
        data,
        filename: format!("image_url.{ext}"),
        mime,
    })
}

/// 百分号解码。**不把 `+` 当空格**（data URL 里 `+` 是字面量）。
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            match u8::from_str_radix(&text[index + 1..index + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                Err(_) => {}
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// 把所有来源取成字节（远程 URL 会下载）。
pub async fn resolve_sources(
    client: &Client,
    sources: &[ImageSource],
    deadline: Deadline,
) -> Result<Vec<ResolvedImage>, InputError> {
    if sources.is_empty() {
        return Err(InputError::Missing);
    }
    let mut resolved = Vec::with_capacity(sources.len());
    for source in sources {
        resolved.push(match source {
            ImageSource::Inline {
                data,
                filename,
                mime,
            } => {
                check_size(data.len())?;
                normalize(data.clone(), filename.clone(), mime.clone())
            }
            ImageSource::DataUrl(url) => decode_data_url(url)?,
            ImageSource::RemoteUrl(url) => download(client, url, deadline).await?,
        });
    }
    if resolved.is_empty() {
        return Err(InputError::Missing);
    }
    Ok(resolved)
}

/// 用嗅探结果校正 MIME 与扩展名 —— 上游按真实类型处理，声明值不可信。
fn normalize(data: Vec<u8>, filename: String, mime: String) -> ResolvedImage {
    match sniff_mime(&data) {
        Some((real_mime, ext)) => {
            let filename = if filename.is_empty() {
                format!("image.{ext}")
            } else {
                filename
            };
            ResolvedImage {
                data,
                filename,
                mime: real_mime.to_string(),
            }
        }
        None => ResolvedImage {
            data,
            filename,
            mime,
        },
    }
}

async fn download(
    client: &Client,
    url: &str,
    deadline: Deadline,
) -> Result<ResolvedImage, InputError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(InputError::NotHttpUrl);
    }
    let budget = deadline
        .budget("image_fetch", Duration::from_secs(60))
        .unwrap_or(Duration::from_secs(60));
    let headers = vec![
        ("Accept", "image/*,*/*;q=0.8".to_string()),
        ("User-Agent", FETCH_USER_AGENT.to_string()),
    ];
    let response = client
        .get_bytes(url, &headers, Some(budget))
        .await
        .map_err(|error| InputError::Fetch(error.to_string()))?;
    if !(200..300).contains(&response.status) {
        return Err(InputError::Fetch(format!("HTTP {}", response.status)));
    }
    check_size(response.body.len())?;
    let filename = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .filter(|segment| segment.contains('.'))
        .unwrap_or("image_url")
        .to_string();
    let resolved = normalize(response.body, filename, "image/png".to_string());
    if sniff_mime(&resolved.data).is_none() {
        return Err(InputError::NotAnImage);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一个最小的合法 PNG（64x48）。
    fn png_bytes() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(64, 48, image::Rgba([9, 9, 9, 255]));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("编码 PNG");
        buffer.into_inner()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn empty_text_yields_no_source() {
        assert_eq!(sources_from_text("").unwrap(), Vec::new());
        assert_eq!(sources_from_text("   ").unwrap(), Vec::new());
    }

    #[test]
    fn data_url_is_recognised() {
        let sources = sources_from_text("data:image/png;base64,AAAA").unwrap();
        assert!(matches!(sources[0], ImageSource::DataUrl(_)));
    }

    #[test]
    fn http_and_https_are_recognised() {
        let sources = sources_from_text("https://example.com/a.png").unwrap();
        assert_eq!(
            sources[0],
            ImageSource::RemoteUrl("https://example.com/a.png".to_string())
        );
        let sources = sources_from_text("http://example.com/a.png").unwrap();
        assert!(matches!(sources[0], ImageSource::RemoteUrl(_)));
    }

    #[test]
    fn bare_base64_decodes() {
        let png = png_bytes();
        let sources = sources_from_text(&b64(&png)).unwrap();
        match &sources[0] {
            ImageSource::Inline { data, .. } => assert_eq!(data, &png),
            other => panic!("期望 Inline，实际 {other:?}"),
        }
    }

    #[test]
    fn base64_with_whitespace_still_decodes() {
        let png = png_bytes();
        let raw = b64(&png);
        let with_spaces = format!("{}\n  {}", &raw[..10], &raw[10..]);
        let sources = sources_from_text(&with_spaces).unwrap();
        assert!(matches!(sources[0], ImageSource::Inline { .. }));
    }

    #[test]
    fn invalid_base64_is_rejected() {
        let error = sources_from_text("@@@not-base64@@@").unwrap_err();
        assert!(matches!(error, InputError::InvalidBase64(_)), "{error}");
    }

    #[test]
    fn json_string_array_and_nested_object() {
        let png = png_bytes();
        let value = json!([b64(&png), {"image_url": "https://x/y.png"}]);
        let sources = sources_from_json(&value).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(matches!(sources[0], ImageSource::Inline { .. }));
        assert!(matches!(sources[1], ImageSource::RemoteUrl(_)));
    }

    #[test]
    fn json_object_with_b64_and_meta() {
        let png = png_bytes();
        let value = json!({"b64_json": b64(&png), "filename": "cat.png", "mime_type": "image/png"});
        let sources = sources_from_json(&value).unwrap();
        match &sources[0] {
            ImageSource::Inline { filename, mime, .. } => {
                assert_eq!(filename, "cat.png");
                assert_eq!(mime, "image/png");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn json_object_image_url_may_be_object_with_url() {
        let value = json!({"image_url": {"url": "https://x/y.png"}});
        let sources = sources_from_json(&value).unwrap();
        assert!(matches!(sources[0], ImageSource::RemoteUrl(_)));
    }

    #[test]
    fn json_file_id_is_rejected_explicitly() {
        let value = json!({"file_id": "file-123"});
        assert_eq!(
            sources_from_json(&value).unwrap_err(),
            InputError::FileIdUnsupported
        );
    }

    #[test]
    fn decode_data_url_base64() {
        let png = png_bytes();
        let url = format!("data:image/png;base64,{}", b64(&png));
        let resolved = decode_data_url(&url).unwrap();
        assert_eq!(resolved.data, png);
        assert_eq!(resolved.mime, "image/png");
        assert_eq!(resolved.filename, "image_url.png");
    }

    #[test]
    fn decode_data_url_percent_encoded() {
        // 非 base64 的 data URL：按百分号解码，且  不应变成空格
        let resolved = decode_data_url("data:image/png,%89PNG%0D%0A%1A%0A+").unwrap();
        assert_eq!(resolved.mime, "image/png");
        assert_eq!(&resolved.data[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(*resolved.data.last().unwrap(), b'+');
    }

    #[test]
    fn decode_data_url_rejects_non_image_mime() {
        let error = decode_data_url("data:text/plain;base64,AAAA").unwrap_err();
        assert!(matches!(error, InputError::InvalidDataUrl(_)), "{error}");
    }

    #[test]
    fn decode_data_url_rejects_missing_comma() {
        let error = decode_data_url("data:image/png;base64").unwrap_err();
        assert!(matches!(error, InputError::InvalidDataUrl(_)));
    }

    #[test]
    fn size_limit_is_enforced() {
        assert!(check_size(0).is_err());
        assert!(check_size(MAX_IMAGE_BYTES).is_ok());
        assert!(matches!(
            check_size(MAX_IMAGE_BYTES + 1),
            Err(InputError::TooLarge { .. })
        ));
    }

    #[test]
    fn normalize_fixes_wrong_declared_mime() {
        let png = png_bytes();
        // 声明成 jpeg，嗅探出 png —— 应以真实类型为准
        let resolved = normalize(png.clone(), "x.jpg".to_string(), "image/jpeg".to_string());
        assert_eq!(resolved.mime, "image/png");
        assert_eq!(resolved.filename, "x.jpg", "文件名保持用户给的");
    }

    #[tokio::test]
    async fn resolve_rejects_empty_source_list() {
        let client = Client::new(Default::default()).expect("客户端");
        let error = resolve_sources(&client, &[], Deadline::in_secs(5.0))
            .await
            .unwrap_err();
        assert_eq!(error, InputError::Missing);
    }

    #[tokio::test]
    async fn resolve_inline_without_network() {
        let client = Client::new(Default::default()).expect("客户端");
        let png = png_bytes();
        let sources = vec![ImageSource::Inline {
            data: png.clone(),
            filename: "a.png".to_string(),
            mime: "image/png".to_string(),
        }];
        let resolved = resolve_sources(&client, &sources, Deadline::in_secs(5.0))
            .await
            .expect("本地内联不需要网络");
        assert_eq!(resolved[0].data, png);
        assert_eq!(resolved[0].mime, "image/png");
    }
}

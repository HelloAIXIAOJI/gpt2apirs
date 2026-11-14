//! 输出图片格式转换（PNG / JPEG / WebP）。
//!
//! 上游只给 PNG。用户要 JPEG/WebP 时在这里转一次，
//! 落盘与 `b64_json` 都用转换后的字节，避免同一张图两套内容。

use std::io::Cursor;

use anyhow::{Context, Result};
use image::ImageEncoder;

/// 对外声明的输出格式（对应 OpenAI images API 的 `output_format`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// PNG（默认）。
    #[default]
    Png,
    /// JPEG：有损，无 alpha。
    Jpeg,
    /// WebP：无损编码。
    Webp,
    /// GIF：**只能透传**（见 `convert`）。
    ///
    /// 动图的帧无法用单帧编码器合成，所以请求 GIF 时我们不生产 GIF，
    /// 只负责把上游给的 GIF 原样保留。
    Gif,
}

impl OutputFormat {
    /// 解析用户传入的写法。接受 MIME 与常见别名。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "png" | "image/png" => Some(Self::Png),
            "jpeg" | "jpg" | "image/jpeg" => Some(Self::Jpeg),
            "webp" | "image/webp" => Some(Self::Webp),
            "gif" | "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }

    /// 对外 MIME。
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    /// 给提示词用的规范名（`jpeg` 而不是 `jpg`）。
    ///
    /// 与 [`Self::ext`] 分开是必要的：扩展名是给人看文件的，
    /// 规范名是写给上游的指令，写成 `jpg` 会和接口文档对不上。
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    /// 落盘扩展名。
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }
}

/// JPEG 编码质量。
///
/// 90 是「肉眼看不出损失、体积明显小于 PNG」的常见取值；
/// 等真有按图调优的需求再做成设置项。
const JPEG_QUALITY: u8 = 90;

/// 把格式/帧/尺寸参数拼成上游提示词里的一段指令。
///
/// 上游没有单独的「输出格式」「输出尺寸」参数，只能靠提示词里的这几句表达意图；
/// 帧参数同理。没给的值**不补默认**——凭空补一个尺寸等于替用户改需求。
/// 写了尺寸也**只是倾向**：上游仍会按提示词整体自适应，不保证精确。
pub fn prompt_hints(
    format: Option<OutputFormat>,
    frame_count: Option<u32>,
    frame_duration_ms: Option<u32>,
    size: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(format) = format {
        lines.push(format!("生成格式为 {}。", format.label()));
    }
    if let Some(count) = frame_count {
        lines.push(format!("frame_count: {count}"));
    }
    if let Some(duration) = frame_duration_ms {
        lines.push(format!("frame_duration_ms: {duration}"));
    }
    if let Some(size) = size {
        lines.push(format!("输出图片尺寸为 {size}。"));
    }
    lines.join("\n")
}

/// 单边像素上限（与上游常见上限一致；超过就不当作尺寸，避免拼出荒唐的提示词）。
pub const MAX_SIZE_DIMENSION: u32 = 8192;

/// 解析 `size` 参数。
///
/// `auto`、空串、未给 → `None`（不拼提示词）；合法 `宽x高` → 规范化的 `宽x高`；
/// 其余一律报错，交给端点转成 400。**不接受**折角写法的其它单位或小数。
pub fn parse_size(raw: &str) -> Result<Option<String>, String> {
    let text = raw.trim().to_ascii_lowercase();
    if text.is_empty() || text == "auto" {
        return Ok(None);
    }
    let mut parts = text.split('x');
    let (Some(width), Some(height), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("尺寸必须写成 宽x高，收到 {raw:?}"));
    };
    let parse = |value: &str| -> Option<u32> {
        let value = value.trim();
        if value.len() < 2 || value.len() > 5 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value.parse::<u32>().ok()
    };
    let (Some(width), Some(height)) = (parse(width), parse(height)) else {
        return Err(format!("尺寸必须写成 宽x高（各 2~5 位数字），收到 {raw:?}"));
    };
    if width > MAX_SIZE_DIMENSION || height > MAX_SIZE_DIMENSION {
        return Err(format!(
            "尺寸超过上限 {MAX_SIZE_DIMENSION}，收到 {raw:?}"
        ));
    }
    Ok(Some(format!("{width}x{height}")))
}

/// 把提示词与 hint 拼起来。hint 为空时提示词保持原样。
pub fn with_prompt_hints(prompt: &str, hints: &str) -> String {
    if hints.trim().is_empty() {
        return prompt.to_string();
    }
    format!("{}\n\n{hints}", prompt.trim_end())
}

/// 把图片字节转成目标格式。
///
/// 已是目标格式时**原样返回**，不重编码 —— 重编码只会掉质量、变体积。
pub fn convert(bytes: &[u8], target: OutputFormat) -> Result<Vec<u8>> {
    let sniffed = image::guess_format(bytes).with_context(|| "无法识别图片格式")?;

    // 动图不能过单帧编码器：转一次就把动画拍平成一张静图。
    // 两个方向都透传 —— 上游给 GIF 时保留 GIF；要求 GIF 时也不假装能合成。
    if sniffed == image::ImageFormat::Gif || target == OutputFormat::Gif {
        return Ok(bytes.to_vec());
    }
    if sniffed == target_to_image_format(target) {
        return Ok(bytes.to_vec());
    }

    let picture = image::load_from_memory_with_format(bytes, sniffed)
        .with_context(|| "图片解码失败")?;

    match target {
        OutputFormat::Png => {
            let mut out = Cursor::new(Vec::new());
            picture
                .write_to(&mut out, image::ImageFormat::Png)
                .with_context(|| "PNG 编码失败")?;
            Ok(out.into_inner())
        }
        OutputFormat::Jpeg => {
            // JPEG 没有 alpha 通道，必须先降成 RGB，否则编码器直接报错。
            let rgb = picture.to_rgb8();
            let mut out = Vec::new();
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
            encoder
                .encode(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .with_context(|| "JPEG 编码失败")?;
            Ok(out)
        }
        OutputFormat::Webp => {
            let mut out = Vec::new();
            let color = picture.color().into();
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .write_image(
                    picture.as_bytes(),
                    picture.width(),
                    picture.height(),
                    color,
                )
                .with_context(|| "WebP 编码失败")?;
            Ok(out)
        }
        // 上面已提前返回；留这个分支是为了让「新增格式必须显式处理」成为编译期约束，
        // 而不是靠 unreachable!() 埋一个 panic。
        OutputFormat::Gif => Ok(bytes.to_vec()),
    }
}

fn target_to_image_format(target: OutputFormat) -> image::ImageFormat {
    match target {
        OutputFormat::Png => image::ImageFormat::Png,
        OutputFormat::Jpeg => image::ImageFormat::Jpeg,
        OutputFormat::Webp => image::ImageFormat::WebP,
        OutputFormat::Gif => image::ImageFormat::Gif,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let picture = image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(picture)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("编码 PNG");
        out.into_inner()
    }

    #[test]
    fn parses_accepted_spellings() {
        assert_eq!(OutputFormat::parse("png"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("PNG"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("image/png"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("jpg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("webp"), Some(OutputFormat::Webp));
        assert_eq!(OutputFormat::parse("bmp"), None, "不支持的格式必须拒绝");
        assert_eq!(OutputFormat::parse(""), None);
    }

    #[test]
    fn png_passes_through_untouched() {
        let original = png_bytes();
        let converted = convert(&original, OutputFormat::Png).expect("转换");
        assert_eq!(converted, original, "已是 PNG 时不应重编码");
    }

    #[test]
    fn jpeg_output_has_jpeg_magic() {
        let converted = convert(&png_bytes(), OutputFormat::Jpeg).expect("转换");
        assert_eq!(&converted[..3], &[0xFF, 0xD8, 0xFF], "JPEG 魔数不对");
    }

    #[test]
    fn webp_output_has_webp_magic() {
        let converted = convert(&png_bytes(), OutputFormat::Webp).expect("转换");
        assert_eq!(&converted[..4], b"RIFF", "WebP 应以 RIFF 开头");
        assert_eq!(&converted[8..12], b"WEBP", "WebP 标识不对");
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(convert(b"not an image", OutputFormat::Jpeg).is_err());
    }

    #[test]
    fn metadata_matches_format() {
        assert_eq!(OutputFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(OutputFormat::Jpeg.ext(), "jpg");
        assert_eq!(OutputFormat::Webp.ext(), "webp");
    }

    /// 最小合法 GIF（1x1，GIF89a）。
    fn gif_bytes() -> Vec<u8> {
        vec![
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00,
            0x3B,
        ]
    }

    #[test]
    fn gif_is_never_transcoded() {
        let original = gif_bytes();
        for target in [OutputFormat::Png, OutputFormat::Jpeg, OutputFormat::Webp] {
            let converted = convert(&original, target).expect("转换");
            assert_eq!(
                converted, original,
                "动图不能被单帧编码器拍平（目标是 {target:?}）"
            );
        }
    }

    #[test]
    fn gif_target_does_not_fake_a_gif() {
        // 要求 GIF 但输入是 PNG：不能凭空合成动图，原样返回
        let original = png_bytes();
        let converted = convert(&original, OutputFormat::Gif).expect("转换");
        assert_eq!(converted, original, "不该声称生成了 GIF");
    }

    #[test]
    fn hints_include_only_given_frame_options() {
        let hints = prompt_hints(Some(OutputFormat::Gif), None, None, None);
        assert_eq!(hints, "生成格式为 gif。");
        let hints = prompt_hints(Some(OutputFormat::Gif), Some(20), None, None);
        assert!(hints.contains("frame_count: 20"));
        assert!(!hints.contains("frame_duration_ms"), "没给就不该补默认值");
        let hints = prompt_hints(Some(OutputFormat::Gif), Some(20), Some(120), None);
        assert!(hints.contains("frame_duration_ms: 120"));
    }

    #[test]
    fn size_parsing_accepts_canonical_forms() {
        assert_eq!(parse_size("1024x1024").unwrap(), Some("1024x1024".to_string()));
        assert_eq!(parse_size(" 1792X1024 ").unwrap(), Some("1792x1024".to_string()));
        assert_eq!(parse_size("auto").unwrap(), None);
        assert_eq!(parse_size("").unwrap(), None);
        assert_eq!(parse_size("   ").unwrap(), None);
    }

    #[test]
    fn size_parsing_rejects_junk() {
        for bad in ["1024", "1024x", "x1024", "1024*1024", "10.5x10", "1x1", "99999x99999"] {
            assert!(parse_size(bad).is_err(), "{bad} 应被拒绝");
        }
        assert!(parse_size("9000x100").is_err(), "超上限应被拒绝");
    }

    #[test]
    fn size_goes_into_prompt_hints() {
        let hints = prompt_hints(None, None, None, Some("1024x1024"));
        assert_eq!(hints, "输出图片尺寸为 1024x1024。");
        // 和格式/帧共存时顺序稳定：格式 → 帧 → 尺寸
        let hints = prompt_hints(Some(OutputFormat::Gif), Some(20), None, Some("512x512"));
        assert!(hints.starts_with("生成格式为 gif。"), "{hints}");
        assert!(hints.contains("frame_count: 20"), "{hints}");
        assert!(hints.ends_with("输出图片尺寸为 512x512。"), "{hints}");
    }

    #[test]
    fn no_size_means_no_size_line() {
        let hints = prompt_hints(Some(OutputFormat::Png), None, None, None);
        assert!(!hints.contains("尺寸"), "没给尺寸就不该出现尺寸句：{hints}");
    }

    #[test]
    fn prompt_keeps_original_prefix() {
        let merged = with_prompt_hints("画一只猫", "生成格式为 gif。");
        assert_eq!(merged, "画一只猫\n\n生成格式为 gif。");
        assert_eq!(with_prompt_hints("画一只猫", ""), "画一只猫");
    }
}

//! mask 合成。
//!
//! OpenAI 的 `/v1/images/edits` 用 mask 表示「哪里要改」：
//!
//! - mask 的**透明区域**（alpha 低）= 需要编辑的地方
//! - mask 的**不透明区域**（alpha 高）= 保留原样的地方
//!
//! 上游只接受一张图，所以要把 mask 的 alpha 通道**合并进原图**再上传，
//! 让上游从 alpha 判断编辑区域。

use anyhow::{Context, Result, bail};

/// 把 mask 的 alpha 通道合成到图片上，输出 PNG。
///
/// `mask` 与 `image` 数量不必一一对应：mask 少于图片时，最后一张 mask 复用到剩下的图片
///
pub fn composite_mask(image: &[u8], mask: &[u8]) -> Result<Vec<u8>> {
    if image.is_empty() {
        bail!("图片内容为空");
    }
    if mask.is_empty() {
        bail!("mask 内容为空");
    }

    let base = image::load_from_memory(image)
        .context("无法解码原图")?
        .to_rgba8();
    let (width, height) = base.dimensions();

    // 只解码一次：拿 alpha 通道同时拿尺寸
    let mask_decoded = image::load_from_memory(mask).context("无法解码 mask")?;
    let (mask_width, mask_height) = (mask_decoded.width(), mask_decoded.height());
    let alpha: Vec<u8> = match &mask_decoded {
        image::DynamicImage::ImageRgba8(rgba) => rgba.pixels().map(|pixel| pixel.0[3]).collect(),
        other => other.to_luma8().into_raw(),
    };
    let source = image::GrayImage::from_raw(mask_width, mask_height, alpha)
        .context("mask alpha 通道尺寸不匹配")?;
    let resized = if (mask_width, mask_height) == (width, height) {
        source
    } else {
        image::imageops::resize(
            &source,
            width,
            height,
            image::imageops::FilterType::Lanczos3,
        )
    };

    let mut composited = base;
    for (index, pixel) in composited.pixels_mut().enumerate() {
        pixel.0[3] = resized.as_raw()[index];
    }

    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(composited)
        .write_to(&mut output, image::ImageFormat::Png)
        .context("编码合成结果失败")?;
    Ok(output.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba(rgba));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("编码 PNG");
        buffer.into_inner()
    }

    /// 左半透明（要改）、右半不透明（保留）。
    fn half_mask(width: u32, height: u32) -> Vec<u8> {
        let mut mask = image::RgbaImage::new(width, height);
        for (x, _y, pixel) in mask.enumerate_pixels_mut() {
            let alpha = if x < width / 2 { 0 } else { 255 };
            *pixel = image::Rgba([255, 255, 255, alpha]);
        }
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(mask)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("编码 mask");
        buffer.into_inner()
    }

    #[test]
    fn composites_alpha_into_image() {
        let image = solid_png(20, 10, [10, 20, 30, 255]);
        let mask = half_mask(20, 10);
        let output = composite_mask(&image, &mask).expect("合成应成功");
        let decoded = image::load_from_memory(&output).expect("解码").to_rgba8();
        assert_eq!(decoded.dimensions(), (20, 10));
        // 左半 alpha = 0（待编辑），右半 alpha = 255（保留）
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0, "左侧应透明");
        assert_eq!(decoded.get_pixel(19, 0).0[3], 255, "右侧应不透明");
        // 颜色不变
        assert_eq!(decoded.get_pixel(0, 0).0[..3], [10, 20, 30]);
    }

    #[test]
    fn mask_is_resized_to_image_size() {
        let image = solid_png(40, 20, [0, 0, 0, 255]);
        let mask = half_mask(20, 10); // 一半尺寸
        let output = composite_mask(&image, &mask).expect("合成应成功");
        let decoded = image::load_from_memory(&output).expect("解码").to_rgba8();
        assert_eq!(decoded.dimensions(), (40, 20), "输出尺寸应跟随原图");
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0);
        assert_eq!(decoded.get_pixel(39, 0).0[3], 255);
    }

    #[test]
    fn grayscale_mask_is_accepted() {
        let image = solid_png(10, 10, [5, 5, 5, 255]);
        let gray = image::GrayImage::from_pixel(10, 10, image::Luma([128]));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(gray)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("编码");
        let output = composite_mask(&image, &buffer.into_inner()).expect("灰度 mask 也应支持");
        let decoded = image::load_from_memory(&output).expect("解码").to_rgba8();
        assert_eq!(decoded.get_pixel(5, 5).0[3], 128);
    }

    #[test]
    fn empty_inputs_are_rejected() {
        assert!(composite_mask(&[], b"x").is_err());
        assert!(composite_mask(b"x", &[]).is_err());
    }

    #[test]
    fn non_image_is_rejected() {
        let image = solid_png(4, 4, [0, 0, 0, 255]);
        assert!(composite_mask(&image, b"not-a-mask").is_err());
    }
}

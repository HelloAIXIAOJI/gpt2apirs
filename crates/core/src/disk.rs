//! 磁盘空间查询。
//!
//! 生成图片前先看剩余空间：空间不足时宁可直接拒绝，也不要「上游出了图、本地存不下」。

use std::path::Path;

/// 指定路径所在卷的可用字节数。
pub fn available_bytes(path: &Path) -> std::io::Result<u64> {
    fs4::available_space(path)
}

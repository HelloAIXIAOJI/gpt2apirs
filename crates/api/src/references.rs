//! 参考图落盘：把这次请求用到的参考图存到 `data/references/YYYY/MM/DD/`。
//!
//! 只在**生成成功之后**才写：生成失败就别在磁盘上留孤儿文件。

use gpt2apirs_storage::repo::NewImageReference;
use uuid::Uuid;

/// 已落盘的一张参考图。
#[derive(Debug, Clone)]
pub struct PersistedReference {
    /// 相对数据目录的路径。
    pub rel_path: String,
    /// MIME。
    pub mime: String,
    /// 字节数。
    pub bytes: i64,
}

/// 把这次请求的参考图逐张落盘；无法识别格式的直接跳过。
pub fn persist_all(items: &[(Vec<u8>, String)]) -> Vec<PersistedReference> {
    let Ok(root) = gpt2apirs_core::paths::data_dir() else {
        return Vec::new();
    };
    let date = chrono::Utc::now().format("%Y/%m/%d").to_string();
    let mut out = Vec::new();
    for (bytes, _name) in items {
        let Some((mime, ext)) = gpt2apirs_generate::sniff_mime(bytes) else {
            continue;
        };
        let rel_path = format!("references/{date}/{}.{ext}", Uuid::new_v4());
        let absolute = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = absolute.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        if std::fs::write(&absolute, bytes).is_err() {
            continue;
        }
        out.push(PersistedReference {
            rel_path,
            mime: mime.to_string(),
            bytes: bytes.len() as i64,
        });
    }
    out
}

/// 把参考图挂到某张生成图上。
pub fn link(image_id: Uuid, items: &[PersistedReference]) -> Vec<NewImageReference> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| NewImageReference {
            image_id,
            ordinal: index as i32 + 1,
            rel_path: item.rel_path.clone(),
            mime: item.mime.clone(),
            bytes: item.bytes,
        })
        .collect()
}

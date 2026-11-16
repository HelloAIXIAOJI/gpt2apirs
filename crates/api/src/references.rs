//! 参考图落盘：内容寻址存到 `data/references/<ab>/<cd>/<sha256>.<ext>`。
//!
//! 同一张图重复上传只存一份；删除时按「还有没有记录引用这个路径」判断，
//! 不会因为共用的参考图被某一张图带走。

use gpt2apirs_storage::repo::NewImageReference;
use sha2::{Digest, Sha256};

/// 已落盘的一张参考图。
#[derive(Debug, Clone)]
pub struct PersistedReference {
    /// 相对数据目录的路径。
    pub rel_path: String,
    /// 内容哈希（十六进制）。
    pub sha256: String,
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
    let mut out = Vec::new();
    for (bytes, _name) in items {
        let Some((mime, ext)) = gpt2apirs_generate::sniff_mime(bytes) else {
            continue;
        };
        let digest = Sha256::digest(bytes);
        let sha256 = hex(&digest);
        let rel_path = format!(
            "references/{}/{}/{}.{ext}",
            &sha256[0..2],
            &sha256[2..4],
            sha256
        );
        let absolute = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !absolute.is_file() {
            if let Some(parent) = absolute.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            if std::fs::write(&absolute, bytes).is_err() {
                continue;
            }
        }
        out.push(PersistedReference {
            rel_path,
            sha256,
            mime: mime.to_string(),
            bytes: bytes.len() as i64,
        });
    }
    out
}

/// 把参考图挂到某张生成图上。
pub fn link(
    image_id: uuid::Uuid,
    items: &[PersistedReference],
) -> Vec<NewImageReference> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| NewImageReference {
            image_id,
            ordinal: index as i32 + 1,
            rel_path: item.rel_path.clone(),
            sha256: item.sha256.clone(),
            mime: item.mime.clone(),
            bytes: item.bytes,
        })
        .collect()
}

/// 删除一张图之后清理参考图文件：只有没人再引用才删。
pub async fn cleanup_after_image_delete(
    state: &std::sync::Arc<crate::state::AppState>,
    image_id: uuid::Uuid,
) {
    let repo = gpt2apirs_storage::repo::ImageReferenceRepo::new(state.db.clone());
    let paths = match repo.paths_for_image(image_id).await {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!(%error, "读取参考图清单失败");
            return;
        }
    };
    let Ok(root) = gpt2apirs_core::paths::data_dir() else {
        return;
    };
    for rel_path in paths {
        match repo.count_by_path(&rel_path).await {
            Ok(0) => {
                let absolute = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
                match std::fs::remove_file(&absolute) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(%error, path = %absolute.display(), "删除参考图失败");
                    }
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "统计参考图引用失败"),
        }
    }
}

/// 扫描 `data/references`，删掉没有任何记录引用的文件。返回删除个数。
pub async fn collect_garbage(
    state: &std::sync::Arc<crate::state::AppState>,
) -> anyhow::Result<usize> {
    let root = gpt2apirs_core::paths::data_dir()?;
    let repo = gpt2apirs_storage::repo::ImageReferenceRepo::new(state.db.clone());
    let referenced = repo.referenced_paths().await?;
    let keep: std::collections::HashSet<String> = referenced.into_iter().collect();
    let dir = root.join("references");
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    let mut stack = vec![dir];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let key = relative.to_string_lossy().replace('\\', "/");
            if keep.contains(&key) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

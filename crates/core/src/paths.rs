//! 路径解析：数据目录到底在哪。
//!
//! **永不使用 CWD**：同一个二进制换个启动方式（systemd / Windows 服务 / 计划任务），
//! CWD 就会变，导致「昨天能读到数据，今天说找不到」。
//!
//! 规则：
//! 1. `DATA_DIR` 已设置 → 绝对路径直接使用；相对路径相对**可执行文件目录**解析
//! 2. 未设置 → `<可执行文件目录>/data`
//!
//! 开发时 exe 在 `target/debug/`，因此那里的 `.env` 写 `DATA_DIR=../../data`。

use std::path::{Component, Path, PathBuf};

use crate::env::Lookup;

/// 数据目录环境变量名。
pub const DATA_DIR_ENV: &str = "DATA_DIR";

/// 未设置 `DATA_DIR` 时的默认子目录名。
pub const DEFAULT_DATA_DIR_NAME: &str = "data";

/// 路径解析错误。
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// 找不到可执行文件目录。
    #[error("无法定位可执行文件目录：{reason}")]
    NoExeDir {
        /// 原因。
        reason: String,
    },

    /// `DATA_DIR` 值不合法。
    #[error("环境变量 DATA_DIR 的值不合法：{value:?}")]
    InvalidDataDir {
        /// 原始值。
        value: String,
    },

    /// 目录创建失败。
    #[error(
        "无法创建数据目录 {path}：{source}。\n\
         如果可执行文件所在目录不可写，请在 .env 中设置 DATA_DIR 指向可写目录，例如：\n\
         \x20 DATA_DIR=/var/lib/gpt2apirs"
    )]
    CreateDir {
        /// 目标路径。
        path: String,
        /// 底层错误。
        #[source]
        source: std::io::Error,
    },
}

/// 可执行文件所在目录。
pub fn exe_dir() -> Result<PathBuf, PathError> {
    let exe = std::env::current_exe().map_err(|err| PathError::NoExeDir {
        reason: err.to_string(),
    })?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| PathError::NoExeDir {
        reason: format!("{} 没有父目录", exe.display()),
    })
}

/// 用进程环境解析数据目录。
pub fn data_dir() -> Result<PathBuf, PathError> {
    // 确保 .env 已加载，否则 DATA_DIR 读不到
    crate::env::ensure_loaded();
    let exe = exe_dir()?;
    resolve_data_dir_from(&exe, &crate::env::read_var)
}

/// **核心规则**：给定 exe 目录与查找函数，算出数据目录。
///
/// 抽出来是为了能在不碰真实环境的前提下测试。
pub fn resolve_data_dir_from(
    exe_dir: &Path,
    lookup: &Lookup<'_>,
) -> Result<PathBuf, PathError> {
    match lookup(DATA_DIR_ENV) {
        Some(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(exe_dir.join(DEFAULT_DATA_DIR_NAME));
            }
            let candidate = Path::new(raw);
            if candidate.is_absolute() {
                Ok(normalize(candidate))
            } else {
                Ok(normalize(&exe_dir.join(candidate)))
            }
        }
        None => Ok(exe_dir.join(DEFAULT_DATA_DIR_NAME)),
    }
}

/// 词法规范化：消除 `.` 与 `..`，不访问文件系统。
///
/// 不解析符号链接（那需要 IO，且会让结果依赖磁盘状态）。
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // 上一层是普通目录  回退
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // 已经在根上，`..` 无效，忽略（POSIX 语义）
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                // 相对路径开头，无法消解，保留
                _ => out.push(component.as_os_str()),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// 确保目录存在（含父目录），返回该路径。
pub fn ensure_dir(path: &Path) -> Result<PathBuf, PathError> {
    std::fs::create_dir_all(path).map_err(|source| PathError::CreateDir {
        path: path.display().to_string(),
        source,
    })?;
    Ok(path.to_path_buf())
}

/// 确保数据目录存在并返回。
pub fn ensure_data_dir() -> Result<PathBuf, PathError> {
    let dir = data_dir()?;
    ensure_dir(&dir)
}

/// `<data>/images`
pub fn images_dir() -> Result<PathBuf, PathError> {
    ensure_dir(&data_dir()?.join("images"))
}

/// `<data>/image_thumbnails`
pub fn thumbnails_dir() -> Result<PathBuf, PathError> {
    ensure_dir(&data_dir()?.join("image_thumbnails"))
}

/// `<data>/logs`
pub fn logs_dir() -> Result<PathBuf, PathError> {
    ensure_dir(&data_dir()?.join("logs"))
}

/// `<data>/capture`（诊断采集产物，非业务数据）
pub fn capture_dir() -> Result<PathBuf, PathError> {
    ensure_dir(&data_dir()?.join("capture"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            map.get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        }
    }

    #[test]
    fn unset_uses_exe_dir_data() {
        let exe = Path::new("/opt/app/bin");
        let l = lookup(HashMap::new());
        let dir = resolve_data_dir_from(exe, &l).unwrap();
        assert_eq!(dir, PathBuf::from("/opt/app/bin/data"));
    }

    #[test]
    fn absolute_data_dir_wins() {
        let exe = Path::new("/opt/app/bin");
        let l = lookup(HashMap::from([("DATA_DIR", "/var/lib/gpt2apirs")]));
        let dir = resolve_data_dir_from(exe, &l).unwrap();
        assert_eq!(dir, PathBuf::from("/var/lib/gpt2apirs"));
    }

    #[test]
    fn relative_data_dir_resolves_against_exe_dir() {
        let exe = Path::new("/opt/app/bin");
        let l = lookup(HashMap::from([("DATA_DIR", "./storage")]));
        let dir = resolve_data_dir_from(exe, &l).unwrap();
        assert_eq!(dir, PathBuf::from("/opt/app/bin/storage"));
    }

    #[test]
    fn dev_style_parent_relative_is_normalized() {
        // 模拟 target/debug + ../../data  仓库根/data
        let exe = Path::new("/repo/target/debug");
        let l = lookup(HashMap::from([("DATA_DIR", "../../data")]));
        let dir = resolve_data_dir_from(exe, &l).unwrap();
        assert_eq!(dir, PathBuf::from("/repo/data"));
    }

    #[test]
    fn normalize_removes_dot_and_parent() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("a/b/../../c")), PathBuf::from("c"));
    }

    #[test]
    fn normalize_keeps_leading_parent_when_unresolvable() {
        // 相对路径开头的 .. 无法消解，必须保留
        assert_eq!(normalize(Path::new("../../x")), PathBuf::from("../../x"));
    }

    #[test]
    fn normalize_does_not_escape_root() {
        assert_eq!(normalize(Path::new("/../..")), PathBuf::from("/"));
    }

    #[test]
    fn empty_data_dir_falls_back_to_default() {
        let exe = Path::new("/opt/app");
        let l = lookup(HashMap::from([("DATA_DIR", "   ")]));
        let dir = resolve_data_dir_from(exe, &l).unwrap();
        assert_eq!(dir, PathBuf::from("/opt/app/data"));
    }

    #[test]
    fn ensure_dir_creates_nested_paths() {
        let base = std::env::temp_dir().join(format!("gpt2apirs-paths-{}", crate::util::new_uuid()));
        let nested = base.join("a/b/c");
        ensure_dir(&nested).expect("应能创建");
        assert!(nested.is_dir());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn ensure_dir_reports_unwritable_path() {
        // 用一个不可能创建的路径（父级是文件）
        let base = std::env::temp_dir().join(format!("gpt2apirs-file-{}", crate::util::new_uuid()));
        std::fs::write(&base, b"x").unwrap();
        let err = ensure_dir(&base.join("child")).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("DATA_DIR"), "错误信息应给出可操作建议：{text}");
        std::fs::remove_file(&base).ok();
    }
}

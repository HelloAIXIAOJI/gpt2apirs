//! `.env` 定位与加载。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 取值函数：给定键，返回已去空白的值。
pub type Lookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// `.env` 相关的错误。
#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    /// 缺少必填项。
    #[error("缺少必填环境变量 {name}。{hint}")]
    Missing {
        /// 变量名。
        name: &'static str,
        /// 可操作的解决建议。
        hint: String,
    },

    /// 值非法。
    #[error("环境变量 {name} 的值非法：{reason}（当前值：{value}）")]
    Invalid {
        /// 变量名。
        name: &'static str,
        /// 当前值（已脱敏）。
        value: String,
        /// 原因。
        reason: String,
    },

    /// 加载 `.env` 失败。
    #[error("加载 {path} 失败：{source}")]
    Load {
        /// 文件路径。
        path: String,
        /// 底层错误。
        #[source]
        source: dotenvy::Error,
    },
}

/// 在可执行文件同目录、再退回 CWD 找 `.env`。
pub fn find_env_file() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let cwd_candidate = PathBuf::from(".env");
    if cwd_candidate.is_file() {
        return Some(cwd_candidate);
    }
    None
}

/// 加载 `.env`（若存在）。
///
/// 返回实际加载的路径，便于日志记录。**不覆盖**进程已有的环境变量。
pub fn load_dotenv() -> Result<Option<PathBuf>, EnvError> {
    let Some(path) = find_env_file() else {
        return Ok(None);
    };
    dotenvy::from_path(&path).map_err(|source| EnvError::Load {
        path: path.display().to_string(),
        source,
    })?;
    Ok(Some(path))
}

static LOADED: OnceLock<Option<PathBuf>> = OnceLock::new();

/// **幂等**加载 `.env`：首次调用真正执行，之后返回缓存结果。
///
/// 目的是让「任何用到环境变量的地方」都能安全地先确保 `.env` 已加载，
/// 而不必要求每个入口都记得手动调 `load_dotenv()`。
///
/// 加载失败不 panic（如实返回 `None`），由调用方在缺变量时报错。
pub fn ensure_loaded() -> Option<PathBuf> {
    LOADED
        .get_or_init(|| match load_dotenv() {
            Ok(found) => found,
            Err(err) => {
                eprintln!("警告：加载 .env 失败：{err}");
                None
            }
        })
        .clone()
}

/// 读一个变量并去掉首尾空白；空字符串视同不存在。
pub fn read_var(key: &str) -> Option<String> {
    ensure_loaded();
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 读必填变量。
pub fn read_required(key: &'static str, hint: &str) -> Result<String, EnvError> {
    read_var(key).ok_or_else(|| EnvError::Missing {
        name: key,
        hint: hint.to_string(),
    })
}

/// 读可选变量并解析成目标类型。
pub fn read_parsed<T>(key: &'static str, default: T) -> Result<T, EnvError>
where
    T: std::str::FromStr,
{
    let Some(raw) = read_var(key) else {
        return Ok(default);
    };
    raw.parse::<T>().map_err(|_| EnvError::Invalid {
        name: key,
        value: raw,
        reason: "无法解析".to_string(),
    })
}

/// 从给定的查找函数读必填项（可测试版本）。
pub fn required_from(
    lookup: &Lookup<'_>,
    key: &'static str,
    hint: &str,
) -> Result<String, EnvError> {
    lookup(key).ok_or_else(|| EnvError::Missing {
        name: key,
        hint: hint.to_string(),
    })
}

/// 从给定的查找函数读可选项并解析（可测试版本）。
pub fn parsed_from<T>(lookup: &Lookup<'_>, key: &'static str, default: T) -> Result<T, EnvError>
where
    T: std::str::FromStr,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    raw.parse::<T>().map_err(|_| EnvError::Invalid {
        name: key,
        value: raw,
        reason: "无法解析".to_string(),
    })
}

/// 校验 `DATABASE_URL` 的 scheme 是否为 PostgreSQL。
pub fn validate_postgres_url(url: &str) -> Result<(), EnvError> {
    let lowered = url.to_ascii_lowercase();
    if lowered.starts_with("postgres://") || lowered.starts_with("postgresql://") {
        return Ok(());
    }
    Err(EnvError::Invalid {
        name: "DATABASE_URL",
        value: redact_url(url),
        reason: "必须以 postgres:// 或 postgresql:// 开头（本项目只支持 PostgreSQL）".to_string(),
    })
}

/// 把 URL 里的密码替换成 `***`。
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let Some(at) = rest.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &rest[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    format!(
        "{}://{}:***@{}",
        &url[..scheme_end],
        &userinfo[..colon],
        &rest[at + 1..]
    )
}

/// 判断路径是否看起来像 `.env` 文件。
pub fn is_env_file(path: &Path) -> bool {
    path.file_name().map(|n| n == ".env").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key: &str| map.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    #[test]
    fn required_from_returns_value() {
        let map = HashMap::from([("TEST_KEY", "abc12345")]);
        let lookup = lookup_from(map);
        assert_eq!(required_from(&lookup, "TEST_KEY", "").unwrap(), "abc12345");
    }

    #[test]
    fn required_from_errors_with_hint() {
        let map = HashMap::new();
        let lookup = lookup_from(map);
        let err = required_from(&lookup, "DATABASE_URL", "请在 .env 里填写").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("DATABASE_URL"));
        assert!(text.contains("请在 .env 里填写"));
    }

    #[test]
    fn parsed_from_uses_default_when_missing() {
        let map = HashMap::new();
        let lookup = lookup_from(map);
        assert_eq!(parsed_from::<u16>(&lookup, "PORT", 8000).unwrap(), 8000);
    }

    #[test]
    fn parsed_from_reports_invalid_value() {
        let map = HashMap::from([("PORT", "abc")]);
        let lookup = lookup_from(map);
        let err = parsed_from::<u16>(&lookup, "PORT", 8000).unwrap_err();
        assert!(err.to_string().contains("PORT"));
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn postgres_url_validation() {
        assert!(validate_postgres_url("postgres://u:p@h:5432/d").is_ok());
        assert!(validate_postgres_url("postgresql://u:p@h:5432/d").is_ok());
        assert!(validate_postgres_url("mysql://u:p@h:3306/d").is_err());
        assert!(validate_postgres_url("sqlite:///data/x.db").is_err());
    }

    #[test]
    fn url_redaction_hides_password() {
        assert_eq!(
            redact_url("postgres://user:secret@host:5432/db?sslmode=disable"),
            "postgres://user:***@host:5432/db?sslmode=disable"
        );
        // 无密码
        assert_eq!(
            redact_url("postgres://user@host:5432/db"),
            "postgres://user@host:5432/db"
        );
    }

    #[test]
    fn env_file_detection() {
        assert!(is_env_file(Path::new("C:/x/.env")));
        assert!(!is_env_file(Path::new("C:/x/.env.local")));
    }
}

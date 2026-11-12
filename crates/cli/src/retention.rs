//! 保留期清理：让 `log_retention_hours` / `image_retention_hours` 真正生效。
//!
//! 每小时跑一次：删过期调用日志、删过期图片文件。
//! 这两个设置此前只存在于契约表里、没有任何消费方。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_storage::repo::CallRecordRepo;

/// 清理间隔。
const INTERVAL: Duration = Duration::from_secs(3600);

/// 文件是否已过期。
pub fn is_expired(modified: SystemTime, now: SystemTime, hours: i64) -> bool {
    if hours <= 0 {
        return false;
    }
    match now.duration_since(modified) {
        Ok(age) => age > Duration::from_secs(hours as u64 * 3600),
        // 修改时间在未来（时钟漂移）：不动它，宁可少删
        Err(_) => false,
    }
}

/// 删掉某个目录下过期的文件，返回 (删除数, 释放字节)。
pub fn purge_dir(dir: &Path, hours: i64, now: SystemTime) -> (u64, u64) {
    let mut removed = 0u64;
    let mut freed = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let (nested_removed, nested_freed) = purge_dir(&path, hours, now);
            removed += nested_removed;
            freed += nested_freed;
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        let Ok(modified) = metadata.modified() else { continue };
        if is_expired(modified, now, hours) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
            freed += metadata.len();
        }
    }
    (removed, freed)
}

/// 执行一轮清理。
pub async fn run_once(
    settings: &RuntimeSettings,
    repo: &CallRecordRepo,
    images_dir: &Path,
) -> (u64, u64, u64) {
    let log_hours = settings.get_i64("log_retention_hours");
    let image_hours = settings.get_i64("image_retention_hours");

    let logs_removed = match repo.purge_older_than(log_hours).await {
        Ok(count) => count,
        Err(error) => {
            tracing::warn!(%error, "清理调用日志失败");
            0
        }
    };
    let (images_removed, images_freed) = purge_dir(images_dir, image_hours, SystemTime::now());

    if logs_removed > 0 || images_removed > 0 {
        tracing::info!(
            logs_removed,
            images_removed,
            images_freed_bytes = images_freed,
            log_hours,
            image_hours,
            "保留期清理完成"
        );
    }
    (logs_removed, images_removed, images_freed)
}

/// 启动后台清理任务。
pub fn spawn(settings: Arc<RuntimeSettings>, pool: sqlx::PgPool, images_dir: std::path::PathBuf) {
    tokio::spawn(async move {
        let repo = CallRecordRepo::new(pool);
        loop {
            let _ = run_once(&settings, &repo, &images_dir).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_boundary() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(48 * 3600);
        assert!(is_expired(old, now, 24));
        assert!(!is_expired(old, now, 72));
        assert!(!is_expired(old, now, 0), "0 表示不清理");
    }

    #[test]
    fn future_mtime_is_kept() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        assert!(!is_expired(future, now, 1), "时钟漂移时宁可不删");
    }
}

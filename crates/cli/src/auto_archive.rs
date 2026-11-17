//! 容量阈值自动归档：在线图片总量超过设置值时，把最老的一批归档并清掉。
//!
//! 默认关闭（阈值为 0 时什么都不做）。这是唯一会「自动删在线图片」的地方，
//! 所以只在明确配置阈值后才运行，且归档链路本身是「写包→校验→删文件→删记录」。

use std::sync::Arc;
use std::time::Duration;

use gpt2apirs_api::state::AppState;

/// 检查间隔。
const INTERVAL: Duration = Duration::from_secs(600);

/// 起一个后台任务，每 10 分钟检查一次。
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let mb = state.settings.get_i64("image_archive_threshold_mb");
            if mb <= 0 {
                continue;
            }
            let threshold = mb as u64 * 1024 * 1024;
            match gpt2apirs_api::image_archive::run_threshold(&state, threshold).await {
                Ok(Some(outcome)) => tracing::info!(
                    file = %outcome.file_name,
                    images = outcome.image_count,
                    bytes = outcome.image_bytes,
                    "自动归档完成"
                ),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "自动归档失败"),
            }
        }
    });
}

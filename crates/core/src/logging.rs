//! 日志初始化：tracing + **脱敏输出**。

use std::collections::HashSet;
use std::fmt;

use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::fmt::format::Writer as FmtWriter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::redact::redact_line;

/// 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// 单行人类可读（开发）。
    Pretty,
    /// JSON 每行一条（生产，便于采集）。
    Json,
}

/// 默认白名单（`LOG_LEVELS` 未配置时的兜底）。
pub fn default_levels() -> HashSet<Level> {
    HashSet::from([Level::INFO, Level::WARN, Level::ERROR])
}

/// 把 `"info,warning,error"` 解析成级别集合。
///
/// 无法识别的项被忽略；结果为空时回退到 [`default_levels`]。
///
/// 注意 Python 用的是 `"warning"`，tracing 里对应 `WARN`——两种写法都接受。
pub fn parse_levels(list: &[String]) -> HashSet<Level> {
    let mut set = HashSet::new();
    for raw in list {
        match raw.trim().to_lowercase().as_str() {
            "trace" => {
                set.insert(Level::TRACE);
            }
            "debug" => {
                set.insert(Level::DEBUG);
            }
            "info" => {
                set.insert(Level::INFO);
            }
            "warning" | "warn" => {
                set.insert(Level::WARN);
            }
            "error" => {
                set.insert(Level::ERROR);
            }
            _ => {}
        }
    }
    if set.is_empty() {
        return default_levels();
    }
    set
}

/// 包一层脱敏的 `FormatEvent`。
pub struct RedactingFormatter<F> {
    inner: F,
}

impl<F> RedactingFormatter<F> {
    /// 用内层 formatter 构造。
    pub fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<S, N, F> FormatEvent<S, N> for RedactingFormatter<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: FmtWriter<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut buffer = String::new();
        self.inner
            .format_event(ctx, FmtWriter::new(&mut buffer), event)?;
        write!(writer, "{}", redact_line(&buffer))
    }
}

/// 初始化全局日志。
///
/// - `rust_log`：`RUST_LOG` 语法（如 `info,gpt2apirs=debug`）
/// - `levels`：白名单（来自 `.env` 的 `LOG_LEVELS`），为空则用默认三档
/// - `format`：输出格式
///
/// 重复调用会返回错误（全局 subscriber 只能设置一次）。
pub fn init_logging(
    rust_log: &str,
    levels: &[String],
    format: LogFormat,
) -> Result<(), anyhow::Error> {
    let env_filter = EnvFilter::try_new(rust_log).unwrap_or_else(|_| EnvFilter::new("info"));
    let allowed = parse_levels(levels);

    let allowed_for_fn = allowed.clone();
    let make_filter = move || {
        let allowed = allowed_for_fn.clone();
        filter_fn(move |metadata: &Metadata<'_>| allowed.contains(metadata.level()))
    };

    match format {
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(RedactingFormatter::new(Format::default()))
                        .with_filter(make_filter()),
                )
                .try_init()?;
        }
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(RedactingFormatter::new(Format::default().json()))
                        .with_filter(make_filter()),
                )
                .try_init()?;
        }
    }
    Ok(())
}

/// 打印加载阶段的告警（此时日志可能已可用了，也可能没有）。
pub fn log_warnings(warnings: &[String]) {
    for warning in warnings {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedVecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedWriter {
        type Writer = SharedVecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            SharedVecWriter(self.0.clone())
        }
    }

    fn capture<F: FnOnce()>(f: F) -> String {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .event_format(RedactingFormatter::new(
                Format::default().without_time().with_ansi(false),
            ))
            .with_writer(SharedWriter(buffer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap_or_default()
    }

    #[test]
    fn parse_levels_accepts_python_names() {
        let levels = vec!["INFO".to_string(), "warning".to_string(), "Error".to_string()];
        let set = parse_levels(&levels);
        assert!(set.contains(&Level::INFO));
        assert!(set.contains(&Level::WARN));
        assert!(set.contains(&Level::ERROR));
        assert!(!set.contains(&Level::DEBUG));
    }

    #[test]
    fn parse_levels_falls_back_to_default() {
        let set = parse_levels(&[]);
        assert_eq!(set, default_levels());
        let bogus = parse_levels(&["nonsense".to_string()]);
        assert_eq!(bogus, default_levels());
    }

    #[test]
    fn formatter_redacts_long_token_field() {
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{}", "A".repeat(200));
        let token_clone = token.clone();
        let output = capture(move || {
            tracing::info!(access_token = %token_clone, "request done");
        });
        assert!(
            !output.contains(&"A".repeat(200)),
            "长 token 未被脱敏，输出：{output}"
        );
        assert!(output.contains("request done"), "普通消息不应被破坏：{output}");
    }

    #[test]
    fn formatter_redacts_inline_base64() {
        let blob = format!("data:image/png;base64,{}", "C".repeat(300));
        let blob_clone = blob.clone();
        let output = capture(move || {
            tracing::info!("body payload {blob_clone}");
        });
        assert!(
            !output.contains(&"C".repeat(300)),
            "内嵌 data URL 未被脱敏：{output}"
        );
    }

    #[test]
    fn formatter_keeps_normal_logs() {
        let output = capture(|| {
            tracing::info!(status = 200, path = "/v1/images/generations", "ok");
        });
        assert!(output.contains("status=200"), "普通字段应保留：{output}");
        assert!(output.contains("/v1/images/generations"), "路径应保留：{output}");
    }

    #[test]
    fn capture_helper_works() {
        // 自检：确认测试脚手架本身能抓到输出
        let output = capture(|| {
            tracing::warn!("hello {}", "world");
        });
        let mut buf = String::new();
        let _ = write!(&mut buf, "{output}");
        assert!(buf.contains("hello world"));
    }
}

//! 管理面登录失败节流：同来源连续失败到阈值后锁一段时间。
//!
//! 记录只在内存里，进程重启即清零——登录失败计数不值得为它引入持久化与
//! 跨实例协调。来源取 TCP 对端地址：管理面不该直接暴露公网，挡在反代后面时
//! 所有请求会落到同一个地址上，这是刻意的取舍（宁可锁住这个来源，也不信任
//! 可伪造的 `X-Forwarded-For`）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use gpt2apirs_core::AppError;

use crate::AppState;
use crate::error::ApiError;

/// 触发锁定所需的连续失败次数。
pub const MAX_FAILURES: u32 = 5;
/// 失败计数的滑动窗口。
pub const WINDOW: Duration = Duration::from_secs(600);
/// 锁定时长。
pub const LOCKOUT: Duration = Duration::from_secs(900);
/// 记录条数上限，防来源地址被打爆内存。
const MAX_ENTRIES: usize = 4096;

/// 放行与否。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 可以尝试登录。
    Allow,
    /// 仍在锁定期；秒数是建议等待时间。
    Locked {
        /// 建议等待秒数。
        retry_after_secs: u64,
    },
}

#[derive(Debug)]
struct Attempt {
    failures: u32,
    first_failure: Instant,
    locked_until: Option<Instant>,
}

/// 登录失败计数器（按来源地址）。
#[derive(Debug, Default)]
pub struct LoginThrottle {
    inner: Mutex<HashMap<String, Attempt>>,
}

impl LoginThrottle {
    /// 新建一个空计数器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试登录前调用：锁定期内直接拒绝。
    pub fn check(&self, source: &str, now: Instant) -> Verdict {
        let Ok(guard) = self.inner.lock() else {
            return Verdict::Allow;
        };
        let Some(attempt) = guard.get(source) else {
            return Verdict::Allow;
        };
        if let Some(until) = attempt.locked_until {
            if until > now {
                return Verdict::Locked {
                    retry_after_secs: until.duration_since(now).as_secs().max(1),
                };
            }
        }
        drop(guard);
        Verdict::Allow
    }

    /// 登录失败后调用。
    pub fn record_failure(&self, source: &str, now: Instant) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if guard.len() >= MAX_ENTRIES && !guard.contains_key(source) {
            prune(&mut guard, now);
        }
        let attempt = guard.entry(source.to_string()).or_insert(Attempt {
            failures: 0,
            first_failure: now,
            locked_until: None,
        });
        if attempt.locked_until.is_some() {
            return;
        }
        if now.duration_since(attempt.first_failure) > WINDOW {
            attempt.failures = 0;
            attempt.first_failure = now;
        }
        attempt.failures += 1;
        if attempt.failures >= MAX_FAILURES {
            attempt.locked_until = Some(now + LOCKOUT);
            tracing::warn!(source = %source, "管理面登录失败次数达到阈值，已锁定");
        }
    }

    /// 登录成功后调用：清掉该来源的失败记录。
    pub fn record_success(&self, source: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(source);
        }
    }
}

/// 登录路由上的中间件：锁定期内直接拒绝，按响应码记成功/失败。
///
/// 放在路由层而不是 handler 里，是因为口令校验在 handler 内部，
/// 由「响应是不是 401」来判定失败更不容易漏（handler 以后换实现也不受影响）。
pub async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let source = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let now = Instant::now();
    if let Verdict::Locked { retry_after_secs } = state.login_throttle.check(&source, now) {
        return ApiError::admin(AppError::RateLimited {
            message: format!("登录失败次数过多，请 {retry_after_secs} 秒后再试"),
            retry_after: Some(retry_after_secs),
        })
        .into_response();
    }
    let response = next.run(request).await;
    match response.status() {
        StatusCode::UNAUTHORIZED => state.login_throttle.record_failure(&source, now),
        status if status.is_success() => state.login_throttle.record_success(&source),
        _ => {}
    }
    response
}

/// 丢掉已经过期且不在锁定期的记录。
fn prune(guard: &mut HashMap<String, Attempt>, now: Instant) {
    guard.retain(|_, attempt| match attempt.locked_until {
        Some(until) => until > now,
        None => now.duration_since(attempt.first_failure) <= WINDOW,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn allows_until_threshold() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES - 1 {
            throttle.record_failure("a", now);
        }
        assert_eq!(throttle.check("a", now), Verdict::Allow);
    }

    #[test]
    fn locks_after_threshold() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES {
            throttle.record_failure("a", now);
        }
        assert!(matches!(throttle.check("a", now), Verdict::Locked { .. }));
        assert_eq!(throttle.check("b", now), Verdict::Allow, "别的来源不受影响");
    }

    #[test]
    fn lock_expires() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES {
            throttle.record_failure("a", now);
        }
        let later = now + LOCKOUT + Duration::from_secs(1);
        assert_eq!(throttle.check("a", later), Verdict::Allow);
    }

    #[test]
    fn success_clears_failures() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES - 1 {
            throttle.record_failure("a", now);
        }
        throttle.record_success("a");
        throttle.record_failure("a", now);
        assert_eq!(throttle.check("a", now), Verdict::Allow, "成功后重新计数");
    }

    #[test]
    fn window_expiry_resets_counter() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES - 1 {
            throttle.record_failure("a", now);
        }
        let later = now + WINDOW + Duration::from_secs(1);
        throttle.record_failure("a", later);
        assert_eq!(throttle.check("a", later), Verdict::Allow, "窗口过期后从头计数");
    }

    #[test]
    fn locked_source_ignores_more_failures() {
        let throttle = LoginThrottle::new();
        let now = at(0);
        for _ in 0..MAX_FAILURES {
            throttle.record_failure("a", now);
        }
        throttle.record_failure("a", now + Duration::from_secs(1));
        let verdict = throttle.check("a", now + Duration::from_secs(1));
        assert!(matches!(verdict, Verdict::Locked { retry_after_secs } if retry_after_secs <= LOCKOUT.as_secs()));
    }
}

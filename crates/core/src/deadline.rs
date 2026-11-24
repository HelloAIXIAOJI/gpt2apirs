//! 全链路截止时间。
//!
//! 总时限 `stream + poll + 30`（两项都来自设置项，默认 `80 + 60 + 30 = 170s`），
//! 再按阶段切成七段预算：
//! **一次请求的总时长必须有上限**，而不是每个阶段各自限时。

use std::time::{Duration, Instant};

/// 一个绝对截止时刻。`None` 表示不限时。
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    at: Option<Instant>,
}

impl Deadline {
    /// 不限时。
    pub fn unlimited() -> Self {
        Self { at: None }
    }

    /// 从**现在**起 `duration` 后到期。
    pub fn after(duration: Duration) -> Self {
        Self {
            at: Some(Instant::now() + duration),
        }
    }

    /// 从**现在**起 `secs` 秒后到期。
    pub fn in_secs(secs: f64) -> Self {
        let clamped = if secs.is_finite() && secs > 0.0 { secs } else { 0.0 };
        Self::after(Duration::from_secs_f64(clamped))
    }

    /// 是否不限时。
    pub fn is_unlimited(&self) -> bool {
        self.at.is_none()
    }

    /// 剩余时间；已过期返回 `None`。
    pub fn remaining(&self) -> Option<Duration> {
        match self.at {
            None => None,
            Some(at) => {
                let now = Instant::now();
                if now >= at {
                    None
                } else {
                    Some(at - now)
                }
            }
        }
    }

    /// 是否已过期。
    pub fn is_expired(&self) -> bool {
        self.at.map(|at| Instant::now() >= at).unwrap_or(false)
    }

    /// 阶段检查：已过期则报错。
    ///
    /// 返回该阶段可用的超时：`min(remaining, cap)`。
    /// 不限时时返回 `cap`。
    pub fn budget(&self, stage: &'static str, cap: Duration) -> Result<Duration, DeadlineExceeded> {
        match self.remaining() {
            None if self.is_unlimited() => Ok(cap),
            None => Err(DeadlineExceeded { stage }),
            Some(left) => {
                if left.is_zero() {
                    Err(DeadlineExceeded { stage })
                } else {
                    Ok(left.min(cap))
                }
            }
        }
    }
}

/// 阶段超时错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("阶段 {stage} 超出总时限")]
pub struct DeadlineExceeded {
    /// 发生在哪个阶段。
    pub stage: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_expires() {
        let d = Deadline::unlimited();
        assert!(d.is_unlimited());
        assert!(!d.is_expired());
        assert_eq!(d.remaining(), None);
        assert_eq!(
            d.budget("sse", Duration::from_secs(80)).unwrap(),
            Duration::from_secs(80)
        );
    }

    #[test]
    fn zero_deadline_is_already_expired() {
        let d = Deadline::in_secs(0.0);
        assert!(d.is_expired());
        let err = d.budget("bootstrap", Duration::from_secs(30)).unwrap_err();
        assert_eq!(err.stage, "bootstrap");
    }

    #[test]
    fn negative_and_nan_are_treated_as_expired() {
        assert!(Deadline::in_secs(-5.0).is_expired());
        assert!(Deadline::in_secs(f64::NAN).is_expired());
    }

    #[test]
    fn budget_is_capped_by_remaining() {
        let d = Deadline::in_secs(10.0);
        // 剩余约 10s，cap 60s  应取 10s
        let budget = d.budget("sse", Duration::from_secs(60)).unwrap();
        assert!(budget <= Duration::from_secs(10));
        assert!(budget > Duration::from_secs(9));
    }

    #[test]
    fn budget_uses_cap_when_deadline_is_far() {
        let d = Deadline::in_secs(600.0);
        let budget = d.budget("sse", Duration::from_secs(30)).unwrap();
        assert_eq!(budget, Duration::from_secs(30));
    }

    #[test]
    fn remaining_shrinks_over_time() {
        let d = Deadline::in_secs(5.0);
        let first = d.remaining().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let second = d.remaining().unwrap();
        assert!(second < first, "剩余时间应减少: {first:?} -> {second:?}");
    }
}

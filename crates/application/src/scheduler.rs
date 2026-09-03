//! 后台遥测刷新调度。
//!
//! App Server 的活动通知是事件驱动的；本调度器只决定额度/汇总快照的兜底轮询和失败退避，
//! 不参与状态灯的逐帧动画。

use std::time::Duration;

use codex_taskbar_domain::activity::ActivityState;

/// 可测试的刷新节奏配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    pub active_interval: Duration,
    pub idle_interval: Duration,
    pub first_retry: Duration,
    pub max_retry: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            active_interval: Duration::from_secs(15),
            idle_interval: Duration::from_secs(60),
            first_retry: Duration::from_secs(5),
            max_retry: Duration::from_secs(300),
        }
    }
}

/// 保存连续失败次数，成功后立即恢复正常节奏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshScheduler {
    policy: RefreshPolicy,
    consecutive_failures: u32,
}

impl RefreshScheduler {
    #[must_use]
    pub const fn new(policy: RefreshPolicy) -> Self {
        Self { policy, consecutive_failures: 0 }
    }

    /// 记录一次成功读取并清除退避。
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// 记录失败。计数饱和而不是溢出，长期断网不会产生异常短重试。
    pub fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// 返回下一次遥测快照读取的等待时间。
    #[must_use]
    pub fn next_delay(&self, activity: ActivityState) -> Duration {
        if self.consecutive_failures > 0 {
            let exponent = self.consecutive_failures.saturating_sub(1).min(31);
            let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
            return self.policy.first_retry.saturating_mul(factor).min(self.policy.max_retry);
        }

        if matches!(activity, ActivityState::Thinking | ActivityState::Executing | ActivityState::Reviewing) {
            self.policy.active_interval
        } else {
            self.policy.idle_interval
        }
    }

    #[must_use]
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

impl Default for RefreshScheduler {
    fn default() -> Self {
        Self::new(RefreshPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_and_idle_use_different_snapshot_cadence() {
        let scheduler = RefreshScheduler::default();
        assert_eq!(scheduler.next_delay(ActivityState::Executing), Duration::from_secs(15));
        assert_eq!(scheduler.next_delay(ActivityState::Idle), Duration::from_secs(60));
    }

    #[test]
    fn failures_back_off_and_success_resets_the_counter() {
        let mut scheduler = RefreshScheduler::default();
        let delays = (0..8)
            .map(|_| {
                scheduler.record_failure();
                scheduler.next_delay(ActivityState::Executing)
            })
            .collect::<Vec<_>>();
        assert_eq!(delays[0], Duration::from_secs(5));
        assert_eq!(delays[1], Duration::from_secs(10));
        assert_eq!(*delays.last().unwrap(), Duration::from_secs(300));

        scheduler.record_success();
        assert_eq!(scheduler.consecutive_failures(), 0);
        assert_eq!(scheduler.next_delay(ActivityState::Executing), Duration::from_secs(15));
    }
}

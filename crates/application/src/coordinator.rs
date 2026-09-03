//! 运行时遥测事件到单一 UI 状态的归并器。
//!
//! 后台 App Server 与 SQLite 线程只发送不可变事件；只有本归并器可以修改
//! [`MonitorState`](crate::monitor::MonitorState)，从而避免窗口线程观察到半份快照。

use codex_taskbar_domain::{activity::ActivityState, official::OfficialSnapshot};

use crate::{RateLimitSnapshot, TokenUsageSnapshot, monitor::MonitorState};

/// 后台数据源提交给应用层的最小事件集合。
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryUpdate {
    /// 官方账户 generation 已变化；先原子清除上一账户的缓存，再应用新快照。
    ResetAccountScopedState,
    RateLimits(RateLimitSnapshot),
    Activity {
        states: Vec<ActivityState>,
        observed_at_unix_ms: i64,
    },
    TokenUsage(Box<TokenUsageSnapshot>),
    /// 仅更新本机捕获的今日累计；不得覆盖 App Server 的当前线程用量。
    LocalTodayUsage {
        counts: Option<codex_taskbar_domain::usage::TokenCounts>,
        observed_at_unix_ms: i64,
    },
    Official(Box<OfficialSnapshot>),
    /// 仅额度端点不可用；保留账户与当前线程的独立状态。
    RateLimitsUnavailable,
    /// 权威实时来源不可用；UI 必须立刻停止展示伪实时值。
    AuthoritativeUnavailable {
        observed_at_unix_ms: i64,
    },
}

/// 串行归并遥测事件，并告诉 UI 是否需要提交新绘制模型。
#[derive(Debug, Default)]
pub struct MonitorCoordinator {
    state: MonitorState,
}

impl MonitorCoordinator {
    #[must_use]
    pub fn new(state: MonitorState) -> Self {
        Self { state }
    }

    #[must_use]
    pub const fn state(&self) -> &MonitorState {
        &self.state
    }

    /// 应用事件。返回 `false` 表示旧事件被拒绝或事件没有改变 UI 状态。
    pub fn apply(&mut self, update: TelemetryUpdate) -> bool {
        match update {
            TelemetryUpdate::ResetAccountScopedState => {
                self.state.reset_account_scoped_state();
                true
            }
            TelemetryUpdate::RateLimits(snapshot) => self.state.apply_authoritative(snapshot),
            TelemetryUpdate::Activity { states, observed_at_unix_ms } => {
                let before = self.state.activity;
                self.state.update_activity_at(states, observed_at_unix_ms);
                self.state.activity != before
            }
            TelemetryUpdate::TokenUsage(snapshot) => self.state.apply_token_usage(*snapshot),
            TelemetryUpdate::LocalTodayUsage { counts, observed_at_unix_ms } => {
                self.state.apply_local_today_usage(counts, observed_at_unix_ms)
            }
            TelemetryUpdate::Official(snapshot) => self.state.apply_official(*snapshot),
            TelemetryUpdate::RateLimitsUnavailable => {
                self.state.mark_rate_limits_unavailable();
                true
            }
            TelemetryUpdate::AuthoritativeUnavailable { observed_at_unix_ms } => {
                if observed_at_unix_ms < self.state.observed_at_unix_ms {
                    return false;
                }
                self.state.mark_rate_limits_unavailable();
                self.state.mark_token_usage_unavailable();
                self.state.mark_official_cached();
                // 额度/Token 探针失败不能再把活动状态一起抹掉。活动状态由 App
                // Server 通知与 SQLite freshness 单独维护，API-only 时仍应可用。
                self.state.observed_at_unix_ms = observed_at_unix_ms;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_taskbar_domain::{
        quota::{QuotaPresence, QuotaValue},
        usage::{TokenCounts, UsageSource},
    };

    use super::*;

    #[test]
    fn disconnect_is_one_atomic_ui_transition() {
        let mut coordinator = MonitorCoordinator::default();
        coordinator.apply(TelemetryUpdate::RateLimits(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(10.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(20.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        }));
        coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(12), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::AppServer,
        })));
        coordinator
            .apply(TelemetryUpdate::Activity { states: vec![ActivityState::Executing], observed_at_unix_ms: 100 });

        assert!(coordinator.apply(TelemetryUpdate::AuthoritativeUnavailable { observed_at_unix_ms: 200 }));
        assert_eq!(coordinator.state().weekly.presence, QuotaPresence::Unknown);
        assert!(coordinator.state().token_usage.current_thread.is_none());
        assert_eq!(coordinator.state().activity, ActivityState::Executing);
    }

    #[test]
    fn repeated_activity_does_not_restart_breathing() {
        let mut coordinator = MonitorCoordinator::default();
        assert!(
            coordinator
                .apply(TelemetryUpdate::Activity { states: vec![ActivityState::Executing], observed_at_unix_ms: 100 })
        );
        assert!(
            !coordinator
                .apply(TelemetryUpdate::Activity { states: vec![ActivityState::Executing], observed_at_unix_ms: 200 })
        );
        assert_eq!(coordinator.state().activity_entered_at_unix_ms, 100);
    }

    #[test]
    fn account_reset_event_is_applied_atomically() {
        let mut coordinator = MonitorCoordinator::default();
        coordinator.apply(TelemetryUpdate::RateLimits(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(10.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(20.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        }));
        coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(12), ..TokenCounts::default() }),
            last_turn: Some(TokenCounts { total: Some(3), ..TokenCounts::default() }),
            model_context_window: Some(200_000),
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::AppServer,
        })));
        coordinator
            .apply(TelemetryUpdate::Activity { states: vec![ActivityState::Executing], observed_at_unix_ms: 100 });

        assert!(coordinator.apply(TelemetryUpdate::ResetAccountScopedState));
        assert_eq!(coordinator.state().five_hour.presence, QuotaPresence::Unknown);
        assert!(coordinator.state().five_hour.last_known.is_none());
        assert!(coordinator.state().weekly.last_known.is_none());
        assert_eq!(coordinator.state().token_usage, Default::default());
        assert_eq!(coordinator.state().activity, ActivityState::Executing);
    }
}

//! 监视器状态仲裁。
//!
//! 该模块是外部数据源和 UI 之间的唯一状态入口，负责原子快照、乱序保护和显式降级。

use codex_taskbar_domain::{
    activity::{ActivityState, aggregate},
    official::{OfficialSnapshot, OfficialThreadUsage},
    quota::{QuotaValue, QuotaWindowState},
    usage::{TokenCounts, TokenUsageState, UsageSource},
};

use crate::{RateLimitSnapshot, TokenUsageSnapshot};

/// UI 消费的当前语义状态，不包含任何协议字段或 HWND 信息。
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorState {
    pub five_hour: QuotaWindowState,
    pub weekly: QuotaWindowState,
    pub activity: ActivityState,
    /// 当前活动状态开始的时间，用于三秒呼吸动画，而不是用于数据排序。
    pub activity_entered_at_unix_ms: i64,
    pub token_usage: TokenUsageState,
    /// 官方账户、Credits 与账户级用量。
    pub official: Option<OfficialSnapshot>,
    pub observed_at_unix_ms: i64,
    pub revision: u64,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self {
            five_hour: QuotaWindowState::default(),
            weekly: QuotaWindowState::default(),
            activity: ActivityState::Unknown,
            activity_entered_at_unix_ms: 0,
            token_usage: TokenUsageState::default(),
            official: None,
            observed_at_unix_ms: 0,
            revision: 0,
        }
    }
}

impl MonitorState {
    /// 供展示层读取当前/最近请求线程的官方服务端估算。
    ///
    /// 返回值保留微单位和“估算”语义；展示层不得将缺失的 USD 填为 0，或自行
    /// 按模型价格换算。该数据随官方账户 generation 一起清除。
    #[must_use]
    pub fn official_thread_usage(&self) -> Option<&OfficialThreadUsage> {
        self.official.as_ref()?.account_usage.as_ref()?.thread_usage.as_ref()
    }

    /// 清除与当前官方账户绑定的数据，但保留独立的活动状态。
    ///
    /// 账户切换时不能沿用上一账户的额度或 Token，连 `last_known` 诊断值也必须
    /// 一并丢弃，否则新账户详情卡片会短暂泄露旧账户数据。
    pub fn reset_account_scoped_state(&mut self) {
        self.five_hour = QuotaWindowState::default();
        self.weekly = QuotaWindowState::default();
        self.token_usage = TokenUsageState::default();
        self.official = None;
    }

    /// 原子应用一份 App Server 完整快照。
    ///
    /// 返回 `false` 表示快照比当前状态旧。相同 revision 时使用观测时间打破乱序，避免并行请求晚到后回滚 UI。
    pub fn apply_authoritative(&mut self, snapshot: RateLimitSnapshot) -> bool {
        if snapshot.revision < self.revision
            || (snapshot.revision == self.revision && snapshot.observed_at_unix_ms < self.observed_at_unix_ms)
        {
            return false;
        }

        let five_hour = QuotaWindowState::from_authoritative(snapshot.five_hour, &self.five_hour);
        let weekly = QuotaWindowState::from_authoritative(snapshot.weekly, &self.weekly);

        // 两个窗口和版本号一起提交，禁止 UI 观察到一半来自新快照、一半来自旧快照。
        self.five_hour = five_hour;
        self.weekly = weekly;
        self.observed_at_unix_ms = snapshot.observed_at_unix_ms;
        self.revision = snapshot.revision;
        true
    }

    /// 权威来源断开时将当前值降级为 Unknown，并保留 last-known 供详情诊断。
    pub fn mark_rate_limits_unavailable(&mut self) {
        self.five_hour = QuotaWindowState::unavailable(&self.five_hour);
        self.weekly = QuotaWindowState::unavailable(&self.weekly);
    }

    /// 只在当前窗口 Unknown 时接纳低优先级 session 数据。
    pub fn apply_session_fallback(&mut self, five_hour: Option<QuotaValue>, weekly: Option<QuotaValue>) {
        self.five_hour = self.five_hour.clone().with_session_fallback(five_hour);
        self.weekly = self.weekly.clone().with_session_fallback(weekly);
    }

    /// 聚合当前所有已加载线程的活动状态。
    pub fn update_activity(&mut self, states: impl IntoIterator<Item = ActivityState>) {
        self.update_activity_at(states, self.observed_at_unix_ms);
    }

    /// 聚合活动状态，并只在状态变化时重置呼吸动画起点。
    pub fn update_activity_at(&mut self, states: impl IntoIterator<Item = ActivityState>, observed_at_unix_ms: i64) {
        let activity = aggregate(states);
        if activity != self.activity {
            self.activity = activity;
            self.activity_entered_at_unix_ms = observed_at_unix_ms;
        }
    }

    /// 应用一份 Token 用量快照；较旧观测不得回滚当前统计。
    pub fn apply_token_usage(&mut self, snapshot: TokenUsageSnapshot) -> bool {
        if snapshot.observed_at_unix_ms < self.token_usage.observed_at_unix_ms {
            return false;
        }
        self.token_usage.current_thread = snapshot.current_thread;
        // App Server 的线程累计通知经常不携带 `last_turn`，而本机 session
        // `token_count.last_token_usage` 能提供刚刚弹窗所用的真实输入/缓存/输出
        // 明细。缺字段表示“本次没有更新逐轮明细”，不能把已经捕获到的值清空，
        // 否则弹窗正常、稍后打开详情卡却会全部变成 `--`。账户切换和明确断开
        // 仍分别通过 `reset_account_scoped_state` / `mark_unavailable` 清理旧值。
        if snapshot.last_turn.is_some() {
            self.token_usage.last_turn = snapshot.last_turn;
            self.token_usage.last_turn_source = snapshot.source;
        }
        self.token_usage.model_context_window = snapshot.model_context_window;
        if snapshot.today.is_some() || self.token_usage.today_source != UsageSource::SqliteFallback {
            self.token_usage.today = snapshot.today;
            self.token_usage.today_source = snapshot.source;
            self.token_usage.today_is_partial = false;
        }
        self.token_usage.source = snapshot.source;
        self.token_usage.fresh = snapshot.source.is_realtime();
        self.token_usage.observed_at_unix_ms = snapshot.observed_at_unix_ms;
        if let Some(current) = &self.token_usage.current_thread {
            self.token_usage.last_known = Some(current.clone());
        }
        true
    }

    /// 写入不含线程详情的本机日增量。若官方 App Server 当前已提供权威日聚合，
    /// 不覆盖它；本机值只在官方日用量缺失时作为明确标记的补充。
    pub fn apply_local_today_usage(&mut self, counts: Option<TokenCounts>, observed_at_unix_ms: i64) -> bool {
        if self.token_usage.today.is_some()
            && self.token_usage.today_source == UsageSource::AppServer
            && self.token_usage.fresh
        {
            return false;
        }
        if self.token_usage.today == counts
            && self.token_usage.today_source == UsageSource::SqliteFallback
            && self.token_usage.today_is_partial
        {
            return false;
        }
        self.token_usage.today = counts;
        self.token_usage.today_source = UsageSource::SqliteFallback;
        self.token_usage.today_is_partial = true;
        self.token_usage.observed_at_unix_ms = self.token_usage.observed_at_unix_ms.max(observed_at_unix_ms);
        true
    }

    /// Token 来源断开时清除当前可见值，防止旧计数继续伪装成实时数据。
    pub fn mark_token_usage_unavailable(&mut self) {
        self.token_usage.mark_unavailable();
    }

    /// 应用官方账户详情。额度窗口仍通过 `RateLimitSnapshot` 原子更新，避免重复语义。
    pub fn apply_official(&mut self, snapshot: OfficialSnapshot) -> bool {
        // 官方会话更新由单一后台线程串行发布。端点失败时，新快照的“最后成功时间”
        // 可能早于当前快照，但 Cached/Unavailable 降级仍必须进入 UI，不能被时间排序
        // 拒绝后继续冒充 Live。
        self.official = Some(snapshot);
        true
    }

    /// App Server 断开后保留官方账户最后成功值，但在 UI 中明确显示为缓存。
    pub fn mark_official_cached(&mut self) {
        if let Some(snapshot) = self.official.take() {
            self.official = Some(snapshot.cached());
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_taskbar_domain::official::{OfficialAccountUsage, OfficialSnapshot, OfficialThreadUsage};
    use codex_taskbar_domain::quota::{Freshness, QuotaPresence};
    use codex_taskbar_domain::usage::{TokenCounts, UsageSource};

    use super::*;

    fn quota(used_percent: f32, minutes: u32) -> QuotaValue {
        QuotaValue::from_used_percent(used_percent, Some(minutes), Some(1_800_000_000))
    }

    #[test]
    fn weekly_only_snapshot_atomically_removes_five_hour() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(quota(20.0, 300)),
            weekly: Some(quota(30.0, 10_080)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(quota(31.0, 10_080)),
            observed_at_unix_ms: 200,
            revision: 2,
        });

        assert_eq!(state.five_hour.presence, QuotaPresence::Absent);
        assert_eq!(state.weekly.value.as_ref().map(|value| value.used_percent), Some(31.0));
    }

    #[test]
    fn stale_session_cannot_revive_authoritative_absent() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(quota(30.0, 10_080)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.apply_session_fallback(Some(quota(20.0, 300)), None);

        assert_eq!(state.five_hour.presence, QuotaPresence::Absent);
    }

    #[test]
    fn older_revision_is_ignored() {
        let mut state = MonitorState::default();
        assert!(state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(quota(35.0, 10_080)),
            observed_at_unix_ms: 200,
            revision: 2,
        }));
        assert!(!state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(quota(10.0, 300)),
            weekly: Some(quota(10.0, 10_080)),
            observed_at_unix_ms: 300,
            revision: 1,
        }));

        assert_eq!(state.five_hour.presence, QuotaPresence::Absent);
    }

    #[test]
    fn unavailable_then_fallback_is_stale_and_not_taskbar_visible() {
        let mut state = MonitorState::default();
        state.mark_rate_limits_unavailable();
        state.apply_session_fallback(Some(quota(50.0, 300)), None);

        assert_eq!(state.five_hour.freshness, Freshness::Stale);
        assert!(!state.five_hour.is_taskbar_visible());
    }

    #[test]
    fn older_token_snapshot_cannot_roll_back_usage() {
        let mut state = MonitorState::default();
        let snapshot = |total, observed_at_unix_ms| TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(total), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: None,
            today: None,
            observed_at_unix_ms,
            source: UsageSource::AppServer,
        };
        assert!(state.apply_token_usage(snapshot(20, 200)));
        assert!(!state.apply_token_usage(snapshot(10, 100)));
        assert_eq!(state.token_usage.current_thread.as_ref().and_then(TokenCounts::display_total), Some(20));
    }

    #[test]
    fn token_disconnect_does_not_publish_last_known_as_current() {
        let mut state = MonitorState::default();
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(20), ..TokenCounts::default() }),
            last_turn: Some(TokenCounts { total: Some(5), ..TokenCounts::default() }),
            model_context_window: Some(200_000),
            today: None,
            observed_at_unix_ms: 200,
            source: UsageSource::AppServer,
        });
        state.mark_token_usage_unavailable();
        assert!(state.token_usage.current_thread.is_none());
        assert!(state.token_usage.last_turn.is_none());
        assert_eq!(state.token_usage.model_context_window, None);
        assert_eq!(state.token_usage.last_known.and_then(|value| value.display_total()), Some(20));
    }

    #[test]
    fn session_log_last_turn_is_fresh_for_the_consumption_popup() {
        let mut state = MonitorState::default();
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: None,
            last_turn: Some(TokenCounts {
                input: Some(1_200),
                cached_input: Some(800),
                output: Some(300),
                total: Some(1_500),
                ..TokenCounts::default()
            }),
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 200,
            source: UsageSource::SessionLogFallback,
        });

        assert!(state.token_usage.fresh);
        assert_eq!(state.token_usage.last_turn.as_ref().and_then(|counts| counts.input), Some(1_200));
        // 逐轮 session 事件不能冒充官方的全天账单。
        assert!(state.token_usage.today.is_none());
    }

    #[test]
    fn app_server_update_without_last_turn_preserves_session_detail_for_details_card() {
        let mut state = MonitorState::default();
        let captured_turn = TokenCounts {
            input: Some(1_200),
            cached_input: Some(800),
            output: Some(300),
            total: Some(1_500),
            ..TokenCounts::default()
        };
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: None,
            last_turn: Some(captured_turn.clone()),
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 200,
            source: UsageSource::SessionLogFallback,
        });

        // 额度/线程累计刷新没有逐轮字段时，不得抹掉弹窗已经证明有效的明细。
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(9_000), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: Some(200_000),
            today: None,
            observed_at_unix_ms: 300,
            source: UsageSource::AppServer,
        });

        assert_eq!(state.token_usage.last_turn, Some(captured_turn));
        assert_eq!(state.token_usage.last_turn_source, UsageSource::SessionLogFallback);
        assert!(state.token_usage.fresh);
    }

    #[test]
    fn app_server_disconnect_keeps_independent_session_turn_details() {
        let mut state = MonitorState::default();
        let captured_turn = TokenCounts {
            input: Some(1_200),
            cached_input: Some(800),
            output: Some(300),
            total: Some(1_500),
            ..TokenCounts::default()
        };
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: None,
            last_turn: Some(captured_turn.clone()),
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 200,
            source: UsageSource::SessionLogFallback,
        });

        state.mark_token_usage_unavailable();

        assert_eq!(state.token_usage.last_turn, Some(captured_turn));
        assert_eq!(state.token_usage.last_turn_source, UsageSource::SessionLogFallback);
        assert!(!state.token_usage.fresh);
    }

    #[test]
    fn local_today_usage_does_not_replace_live_thread_usage_and_survives_disconnect() {
        let mut state = MonitorState::default();
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(20), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::AppServer,
        });
        assert!(state.apply_local_today_usage(Some(TokenCounts { total: Some(12), ..TokenCounts::default() }), 101));
        assert_eq!(state.token_usage.current_thread.as_ref().and_then(|counts| counts.total), Some(20));
        assert_eq!(state.token_usage.today.as_ref().and_then(|counts| counts.total), Some(12));
        assert_eq!(state.token_usage.today_source, UsageSource::SqliteFallback);
        assert!(state.token_usage.today_is_partial);

        state.mark_token_usage_unavailable();
        assert_eq!(state.token_usage.today.as_ref().and_then(|counts| counts.total), Some(12));
        assert!(state.token_usage.today_is_partial);
    }

    #[test]
    fn account_reset_clears_current_and_last_known_but_keeps_independent_state() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(quota(20.0, 300)),
            weekly: Some(quota(30.0, 10_080)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(20), ..TokenCounts::default() }),
            last_turn: Some(TokenCounts { total: Some(5), ..TokenCounts::default() }),
            model_context_window: Some(200_000),
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::AppServer,
        });
        state.official = Some(OfficialSnapshot::default());
        state.update_activity_at([ActivityState::Executing], 90);

        state.reset_account_scoped_state();

        assert_eq!(state.five_hour, QuotaWindowState::default());
        assert_eq!(state.weekly, QuotaWindowState::default());
        assert_eq!(state.token_usage, TokenUsageState::default());
        assert!(state.official.is_none());
        assert_eq!(state.activity, ActivityState::Executing);
        assert_eq!(state.activity_entered_at_unix_ms, 90);
    }

    #[test]
    fn official_thread_estimate_is_available_to_the_application_display_interface() {
        let mut state = MonitorState::default();
        state.apply_official(OfficialSnapshot {
            account_usage: Some(OfficialAccountUsage {
                thread_usage: Some(OfficialThreadUsage {
                    thread_id: "thread-current".to_owned(),
                    estimated_usage_usd_micros: Some(1_250),
                    estimated_usage_credits_micros: 3_000,
                    ..OfficialThreadUsage::default()
                }),
                ..OfficialAccountUsage::default()
            }),
            ..OfficialSnapshot::default()
        });

        assert_eq!(state.official_thread_usage().map(|usage| usage.thread_id.as_str()), Some("thread-current"));
        assert_eq!(state.official_thread_usage().and_then(|usage| usage.estimated_usage_usd_micros), Some(1_250));
    }
}

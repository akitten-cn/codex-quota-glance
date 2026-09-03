//! 交给 WebView2 / 原生展示层的只读、脱敏状态快照。
//!
//! 该模块是采集与界面之间唯一允许跨越的结构。它不暴露 App Server 原始 JSON、
//! SQLite 行、线程标识、Prompt 或凭据；展示层不得绕过本模块自行读取数据源。

use codex_taskbar_domain::{
    activity::ActivityState,
    official::{OfficialCredits, OfficialFreshness},
    quota::{Freshness, QuotaPresence, QuotaValue},
    usage::{TokenCounts, UsageSource},
};
use serde::{Deserialize, Serialize};

use crate::monitor::MonitorState;

/// Web UI 协议版本。版本升级时由桥接层拒绝旧页面或旧宿主，避免静默错配字段语义。
pub const TASKBAR_SNAPSHOT_SCHEMA_VERSION: u8 = 1;
/// 本次消耗浮窗的独立协议版本；不能复用完整任务栏快照，避免把账户与累计额度
/// 意外送入仅需要单次增量的短生命周期弹窗。
pub const CONSUMPTION_POPUP_SNAPSHOT_SCHEMA_VERSION: u8 = 2;

/// 页面可直接消费的完整状态；所有字段都是可安全序列化的显示投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskbarSnapshot {
    pub schema_version: u8,
    pub observed_at_unix_ms: i64,
    pub activity: ActivitySnapshot,
    pub quota: QuotaSnapshot,
    pub usage: UsageSnapshot,
    pub account: AccountSnapshot,
    pub reset_card: ResetCardSnapshot,
}

/// 自动弹出的“本次消耗”浮窗快照。
///
/// 数据仅来自 fresh 的 `last_turn` 增量。金额和额度下降并非每轮都能可靠取得，
/// 因此协议不再包含这两项，避免一个无关缺失值让整张弹窗看起来失效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumptionPopupSnapshot {
    pub schema_version: u8,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_hit_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub state: ActivityState,
    pub entered_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub five_hour: QuotaWindowSnapshot,
    pub weekly: QuotaWindowSnapshot,
    pub credits: CreditsSnapshot,
}

/// 一个额度窗口的完整三态。`value=None` 不能按零用量绘制。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowSnapshot {
    pub presence: QuotaPresence,
    pub freshness: Freshness,
    pub value: Option<QuotaValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    /// 官方原始单位的字符串；不添加货币符号，也不擅自换算 USD。
    pub balance: Option<String>,
    /// 任务栏只在订阅额度耗尽的条件满足时展示 Credits。
    pub visible_in_taskbar: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub today: Option<TokenCounts>,
    pub today_source: UsageSource,
    pub today_is_partial: bool,
    /// `cached_input / input`；缺任一字段或输入为 0 时保持 None。
    pub cache_hit_percent: Option<u8>,
    /// 当前或最近 Turn 的服务端估算，仅在官方明确提供时出现。
    pub official_estimated_usage_usd_micros: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub masked_identifier: Option<String>,
    pub plan_type: Option<String>,
    pub auth_mode: Option<String>,
    pub freshness: AccountFreshness,
}

/// 官方账户信息的可展示新鲜度。
///
/// 这是 UI 协议自己的枚举，而不是直接暴露领域层类型，避免展示层被内部模型的
/// 序列化约束或字段演进所绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountFreshness {
    Live,
    Cached,
    Unavailable,
}

impl From<OfficialFreshness> for AccountFreshness {
    fn from(value: OfficialFreshness) -> Self {
        match value {
            OfficialFreshness::Live => Self::Live,
            OfficialFreshness::Cached => Self::Cached,
            OfficialFreshness::Unavailable => Self::Unavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCardSnapshot {
    pub available_count: Option<u64>,
    #[serde(default)]
    pub expiry_times_unix: Vec<i64>,
    pub nearest_expiry_unix: Option<i64>,
}

impl TaskbarSnapshot {
    /// 从应用层的仲裁状态构造 UI 投影。
    ///
    /// 此函数故意不接受外部数据源，保证所有页面看到的是同一时刻、同一规则下的值。
    #[must_use]
    pub fn from_monitor_state(state: &MonitorState) -> Self {
        let official = state.official.as_ref();
        let credits = official.and_then(|snapshot| snapshot.credits.as_ref());
        let reset = official.and_then(|snapshot| snapshot.reset_credits.as_ref());
        let account = official.and_then(|snapshot| snapshot.account.as_ref());
        let today = state.token_usage.today.clone();

        Self {
            schema_version: TASKBAR_SNAPSHOT_SCHEMA_VERSION,
            observed_at_unix_ms: state.observed_at_unix_ms.max(state.token_usage.observed_at_unix_ms),
            activity: ActivitySnapshot { state: state.activity, entered_at_unix_ms: state.activity_entered_at_unix_ms },
            quota: QuotaSnapshot {
                five_hour: quota_window_snapshot(&state.five_hour),
                weekly: quota_window_snapshot(&state.weekly),
                credits: credits_snapshot(credits, state),
            },
            usage: UsageSnapshot {
                cache_hit_percent: cache_hit_percent(today.as_ref()),
                today,
                today_source: state.token_usage.today_source,
                today_is_partial: state.token_usage.today_is_partial,
                official_estimated_usage_usd_micros: state
                    .official_thread_usage()
                    .and_then(|usage| usage.estimated_usage_usd_micros),
            },
            account: AccountSnapshot {
                masked_identifier: account.and_then(|value| value.masked_identifier.clone()),
                plan_type: account.and_then(|value| value.plan_type.clone()),
                auth_mode: account.and_then(|value| value.auth_mode.clone()),
                freshness: official.map_or(OfficialFreshness::Unavailable, |snapshot| snapshot.freshness).into(),
            },
            reset_card: ResetCardSnapshot {
                available_count: reset.map(|value| value.available_count),
                expiry_times_unix: reset.map_or_else(Vec::new, |value| value.expiry_times_unix.clone()),
                nearest_expiry_unix: reset.and_then(|value| value.nearest_expiry_unix),
            },
        }
    }
}

impl ConsumptionPopupSnapshot {
    /// 从同一份仲裁状态构造四项可靠 Token 指标。
    #[must_use]
    pub fn from_monitor_state(state: &MonitorState) -> Self {
        let last_turn = state.token_usage.fresh.then_some(()).and(state.token_usage.last_turn.as_ref());
        let input = last_turn.and_then(|counts| counts.input);
        let cached_input = last_turn.and_then(|counts| counts.cached_input);
        let cache_hit_percent = match (input, cached_input) {
            (Some(input), Some(cached)) if input > 0 => Some(((cached.saturating_mul(100) / input).min(100)) as u8),
            _ => None,
        };
        Self {
            schema_version: CONSUMPTION_POPUP_SNAPSHOT_SCHEMA_VERSION,
            input_tokens: input,
            cached_input_tokens: cached_input,
            output_tokens: last_turn.and_then(|counts| counts.output),
            cache_hit_percent,
        }
    }
}

fn quota_window_snapshot(window: &codex_taskbar_domain::quota::QuotaWindowState) -> QuotaWindowSnapshot {
    QuotaWindowSnapshot { presence: window.presence, freshness: window.freshness, value: window.value.clone() }
}

fn credits_snapshot(credits: Option<&OfficialCredits>, state: &MonitorState) -> CreditsSnapshot {
    let (has_credits, unlimited, balance) =
        credits.map_or((false, false, None), |value| (value.has_credits, value.unlimited, value.balance.clone()));
    let five_hour_exhausted = state.five_hour.is_taskbar_visible()
        && state.five_hour.value.as_ref().is_some_and(|quota| quota.remaining_percent <= 0.0);
    let weekly_exhausted_without_five_hour = state.five_hour.presence == QuotaPresence::Absent
        && state.weekly.is_taskbar_visible()
        && state.weekly.value.as_ref().is_some_and(|quota| quota.remaining_percent <= 0.0);
    let visible_in_taskbar =
        has_credits && !unlimited && balance.is_some() && (five_hour_exhausted || weekly_exhausted_without_five_hour);
    CreditsSnapshot { has_credits, unlimited, balance, visible_in_taskbar }
}

fn cache_hit_percent(counts: Option<&TokenCounts>) -> Option<u8> {
    let counts = counts?;
    let input = counts.input?;
    let cached = counts.cached_input?;
    (input > 0).then(|| ((cached.saturating_mul(100) / input).min(100)) as u8)
}

#[cfg(test)]
mod tests {
    use codex_taskbar_domain::{
        official::{OfficialCredits, OfficialSnapshot, OfficialThreadUsage},
        quota::QuotaValue,
    };

    use super::*;

    fn quota(used: f32, minutes: u32) -> QuotaValue {
        QuotaValue::from_used_percent(used, Some(minutes), Some(1_800_000_000))
    }

    fn state_with_credits() -> MonitorState {
        MonitorState {
            official: Some(OfficialSnapshot {
                credits: Some(OfficialCredits {
                    has_credits: true,
                    unlimited: false,
                    balance: Some("12.50".to_owned()),
                }),
                ..OfficialSnapshot::default()
            }),
            ..MonitorState::default()
        }
    }

    #[test]
    fn credits_appear_only_after_the_applicable_subscription_window_is_exhausted() {
        let mut state = state_with_credits();
        state.five_hour = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(
            Some(quota(99.0, 300)),
            &Default::default(),
        );
        state.weekly = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(
            Some(quota(20.0, 10_080)),
            &Default::default(),
        );
        assert!(!TaskbarSnapshot::from_monitor_state(&state).quota.credits.visible_in_taskbar);

        state.five_hour = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(
            Some(quota(100.0, 300)),
            &state.five_hour,
        );
        assert!(TaskbarSnapshot::from_monitor_state(&state).quota.credits.visible_in_taskbar);

        state.five_hour = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(None, &state.five_hour);
        state.weekly = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(
            Some(quota(100.0, 10_080)),
            &state.weekly,
        );
        assert!(TaskbarSnapshot::from_monitor_state(&state).quota.credits.visible_in_taskbar);
    }

    #[test]
    fn stale_windows_never_make_credits_visible_in_the_taskbar() {
        let mut state = state_with_credits();
        state.five_hour = codex_taskbar_domain::quota::QuotaWindowState::from_authoritative(
            Some(quota(100.0, 300)),
            &Default::default(),
        );
        state.mark_rate_limits_unavailable();
        assert!(!TaskbarSnapshot::from_monitor_state(&state).quota.credits.visible_in_taskbar);
    }

    #[test]
    fn cache_ratio_uses_cached_input_without_double_counting_it() {
        let mut state = MonitorState::default();
        state.token_usage.today = Some(TokenCounts {
            input: Some(42_800),
            cached_input: Some(33_384),
            output: Some(9_416),
            total: Some(52_216),
            ..TokenCounts::default()
        });
        state.token_usage.today_source = UsageSource::SqliteFallback;
        state.token_usage.today_is_partial = true;
        let snapshot = TaskbarSnapshot::from_monitor_state(&state);
        assert_eq!(snapshot.usage.cache_hit_percent, Some(78));
        assert!(snapshot.usage.today_is_partial);
    }

    #[test]
    fn serialized_snapshot_never_contains_the_official_thread_identifier() {
        let state = MonitorState {
            official: Some(OfficialSnapshot {
                account_usage: Some(codex_taskbar_domain::official::OfficialAccountUsage {
                    thread_usage: Some(OfficialThreadUsage {
                        thread_id: "private-thread-id".to_owned(),
                        estimated_usage_usd_micros: Some(1_250),
                        ..OfficialThreadUsage::default()
                    }),
                    ..codex_taskbar_domain::official::OfficialAccountUsage::default()
                }),
                ..OfficialSnapshot::default()
            }),
            ..MonitorState::default()
        };
        let json = serde_json::to_string(&TaskbarSnapshot::from_monitor_state(&state)).expect("快照可序列化");
        assert!(!json.contains("private-thread-id"));
        assert!(json.contains("estimated_usage_usd_micros"));
    }

    #[test]
    fn consumption_popup_uses_only_fresh_last_turn_and_never_relabels_thread_cost() {
        let state = MonitorState {
            token_usage: codex_taskbar_domain::usage::TokenUsageState {
                fresh: true,
                last_turn: Some(TokenCounts {
                    input: Some(1_000),
                    cached_input: Some(750),
                    output: Some(320),
                    ..TokenCounts::default()
                }),
                ..Default::default()
            },
            official: Some(OfficialSnapshot {
                account_usage: Some(codex_taskbar_domain::official::OfficialAccountUsage {
                    thread_usage: Some(OfficialThreadUsage {
                        thread_id: "private-thread-id".to_owned(),
                        estimated_usage_usd_micros: Some(12_345),
                        ..OfficialThreadUsage::default()
                    }),
                    ..Default::default()
                }),
                ..OfficialSnapshot::default()
            }),
            ..MonitorState::default()
        };
        let snapshot = ConsumptionPopupSnapshot::from_monitor_state(&state);
        assert_eq!(snapshot.input_tokens, Some(1_000));
        assert_eq!(snapshot.cache_hit_percent, Some(75));
        let json = serde_json::to_string(&snapshot).expect("快照可序列化");
        assert!(!json.contains("private-thread-id"));
        assert!(!json.contains("12_345"));
        assert!(!json.contains("quota_consumed"));
        assert!(!json.contains("estimated_usage"));
    }
}

//! 额度快照和来源仲裁。
//!
//! 旧版把“明确不存在”和“暂时无数据”都表示为空对象，导致旧 5h 数据被重新补回。本模块使用显式三态，
//! 并规定成功的 App Server 响应必须原子替换整个额度快照。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPresence {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    AppServer,
    SessionFallback,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaValue {
    pub used_percent: f32,
    pub remaining_percent: f32,
    pub window_minutes: Option<u32>,
    pub resets_at_unix: Option<i64>,
}

impl QuotaValue {
    /// 服务端给出的 `usedPercent` 可能短暂越界；领域层统一裁剪，避免绘制负角度或超过整圆。
    #[must_use]
    pub fn from_used_percent(used_percent: f32, window_minutes: Option<u32>, resets_at_unix: Option<i64>) -> Self {
        let used_percent = used_percent.clamp(0.0, 100.0);
        Self { used_percent, remaining_percent: 100.0 - used_percent, window_minutes, resets_at_unix }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowState {
    pub presence: QuotaPresence,
    pub freshness: Freshness,
    pub source: QuotaSource,
    pub value: Option<QuotaValue>,
    /// 只用于详情页说明上次观测，任务栏不得把它冒充为当前额度。
    pub last_known: Option<QuotaValue>,
}

impl Default for QuotaWindowState {
    fn default() -> Self {
        Self {
            presence: QuotaPresence::Unknown,
            freshness: Freshness::Stale,
            source: QuotaSource::None,
            value: None,
            last_known: None,
        }
    }
}

impl QuotaWindowState {
    /// 应用权威完整快照中的一个窗口；`None` 是明确 absent，不是 unknown。
    #[must_use]
    pub fn from_authoritative(value: Option<QuotaValue>, previous: &Self) -> Self {
        match value {
            Some(value) => Self {
                presence: QuotaPresence::Present,
                freshness: Freshness::Fresh,
                source: QuotaSource::AppServer,
                last_known: Some(value.clone()),
                value: Some(value),
            },
            None => Self {
                presence: QuotaPresence::Absent,
                freshness: Freshness::Fresh,
                source: QuotaSource::AppServer,
                value: None,
                last_known: previous.last_known.clone(),
            },
        }
    }

    /// 数据源失败时仅保留诊断用 last-known；当前值转为 unknown，UI 不得绘制伪实时圆环。
    #[must_use]
    pub fn unavailable(previous: &Self) -> Self {
        Self {
            presence: QuotaPresence::Unknown,
            freshness: Freshness::Stale,
            source: QuotaSource::None,
            value: None,
            last_known: previous.value.clone().or_else(|| previous.last_known.clone()),
        }
    }

    /// session 只能填充 unknown，永远不能覆盖权威 RPC 的 absent tombstone。
    #[must_use]
    pub fn with_session_fallback(self, value: Option<QuotaValue>) -> Self {
        if self.presence != QuotaPresence::Unknown {
            return self;
        }
        match value {
            Some(value) => Self {
                presence: QuotaPresence::Present,
                freshness: Freshness::Stale,
                source: QuotaSource::SessionFallback,
                last_known: Some(value.clone()),
                value: Some(value),
            },
            None => self,
        }
    }

    #[must_use]
    pub fn is_taskbar_visible(&self) -> bool {
        self.presence == QuotaPresence::Present && self.freshness == Freshness::Fresh && self.value.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five_hour(used: f32) -> QuotaValue {
        QuotaValue::from_used_percent(used, Some(300), Some(1_800_000_000))
    }

    #[test]
    fn authoritative_absent_cannot_be_revived_by_old_session_data() {
        let old = QuotaWindowState::from_authoritative(Some(five_hour(23.0)), &QuotaWindowState::default());
        let absent = QuotaWindowState::from_authoritative(None, &old);
        let merged = absent.with_session_fallback(Some(five_hour(23.0)));

        assert_eq!(merged.presence, QuotaPresence::Absent);
        assert!(!merged.is_taskbar_visible());
        assert!(merged.last_known.is_some());
    }

    #[test]
    fn fallback_is_marked_stale_when_authoritative_source_is_unknown() {
        let unknown = QuotaWindowState::unavailable(&QuotaWindowState::default());
        let merged = unknown.with_session_fallback(Some(five_hour(50.0)));

        assert_eq!(merged.source, QuotaSource::SessionFallback);
        assert_eq!(merged.freshness, Freshness::Stale);
        assert!(!merged.is_taskbar_visible());
    }

    #[test]
    fn authoritative_restore_makes_window_visible_again() {
        let absent = QuotaWindowState::from_authoritative(None, &QuotaWindowState::default());
        let restored = QuotaWindowState::from_authoritative(Some(five_hour(5.0)), &absent);

        assert_eq!(restored.presence, QuotaPresence::Present);
        assert!(restored.is_taskbar_visible());
    }
}

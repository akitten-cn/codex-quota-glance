//! Codex 活动状态及任务栏灯的稳定语义。

use serde::{Deserialize, Serialize};

/// UI 可直接消费的活动状态。
///
/// `Focused` 不属于这里：窗口是否在前台是用户上下文，不代表 Codex 是否正在运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Unknown,
    Idle,
    Thinking,
    Executing,
    WaitingForUser,
    Reviewing,
    Completed,
    Failed,
}

/// 状态灯的动画策略。
///
/// `Continuous` 只用于正在运行的状态；`Timed` 用于 Idle/Completed 的短暂过渡，结束后保留静态光晕，
/// 从而避免空闲时持续重绘任务栏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreathePolicy {
    None,
    Timed { duration_ms: u32 },
    Continuous,
}

/// 与渲染框架无关的状态灯视觉语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityLampStyle {
    pub color: LampColor,
    pub breathe: BreathePolicy,
    pub glow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LampColor {
    Green,
    Cyan,
    BlueViolet,
    Amber,
    Red,
    Gray,
}

impl ActivityState {
    /// 状态灯的统一样式。具体色值、光晕半径和动画曲线由 Windows 渲染层决定。
    #[must_use]
    pub const fn lamp_style(self) -> ActivityLampStyle {
        match self {
            Self::Idle | Self::Completed => ActivityLampStyle {
                color: LampColor::Green,
                breathe: BreathePolicy::Timed { duration_ms: 3_000 },
                glow: true,
            },
            Self::Thinking => {
                ActivityLampStyle { color: LampColor::BlueViolet, breathe: BreathePolicy::Continuous, glow: true }
            }
            Self::Executing | Self::Reviewing => {
                ActivityLampStyle { color: LampColor::Cyan, breathe: BreathePolicy::Continuous, glow: true }
            }
            Self::WaitingForUser => {
                ActivityLampStyle { color: LampColor::Amber, breathe: BreathePolicy::None, glow: true }
            }
            Self::Failed => ActivityLampStyle { color: LampColor::Red, breathe: BreathePolicy::None, glow: true },
            Self::Unknown => ActivityLampStyle { color: LampColor::Gray, breathe: BreathePolicy::None, glow: false },
        }
    }

    /// 聚合多个线程时的显示优先级，数值越大越需要用户注意。
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Failed => 7,
            Self::WaitingForUser => 6,
            Self::Executing => 5,
            Self::Reviewing => 4,
            Self::Thinking => 3,
            Self::Completed => 2,
            Self::Idle => 1,
            Self::Unknown => 0,
        }
    }
}

/// 从所有已加载线程中选出任务栏灯应显示的单一状态。
#[must_use]
pub fn aggregate(states: impl IntoIterator<Item = ActivityState>) -> ActivityState {
    states.into_iter().max_by_key(|state| state.priority()).unwrap_or(ActivityState::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_for_user_wins_over_background_execution() {
        assert_eq!(aggregate([ActivityState::Executing, ActivityState::WaitingForUser]), ActivityState::WaitingForUser);
    }

    #[test]
    fn idle_and_completed_breathe_briefly_then_keep_a_glow() {
        for state in [ActivityState::Idle, ActivityState::Completed] {
            let style = state.lamp_style();
            assert_eq!(style.color, LampColor::Green);
            assert_eq!(style.breathe, BreathePolicy::Timed { duration_ms: 3_000 });
            assert!(style.glow);
        }
    }

    #[test]
    fn executing_breathes_continuously() {
        assert_eq!(ActivityState::Executing.lamp_style().breathe, BreathePolicy::Continuous);
    }
}

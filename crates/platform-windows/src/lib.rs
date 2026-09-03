//! Windows 原生任务栏发现、透明子窗口宿主与独立诊断探针。
//!
//! 默认宿主使用 Explorer taskbar child + layered per-pixel alpha；独立探针仅供
//! `--probe-plan` 和故障回退相关诊断，不参与常驻 UI。

use thiserror::Error;

use codex_taskbar_domain::layout::TaskbarAnchor;

pub mod geometry;
pub mod host;
pub mod render_model;
pub mod web_snapshot;

#[cfg(not(windows))]
mod portable_time;
#[cfg(not(windows))]
pub use portable_time::{format_local_unix_time, local_usage_clock, local_usage_clock_at};

#[cfg(all(windows, feature = "direct2d"))]
pub mod render;
#[cfg(all(windows, feature = "direct2d"))]
pub use render::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("当前平台不是 Windows")]
    UnsupportedPlatform,
    #[error("未找到目标任务栏")]
    TaskbarNotFound,
    #[error("右侧任务栏缺少通知区域或时钟边界，已拒绝布局以保护屏幕右侧")]
    MissingRightSafetyBoundary,
    #[error("任务栏可用空间不足")]
    InsufficientSpace,
    #[error("Windows 平台调用失败：{0}")]
    Windows(String),
}

/// P0 探针配置。`target_monitor_device` 使用稳定设备名持久化，避免显示器序号变化后跑错屏幕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeConfig {
    pub target_monitor_device: Option<String>,
    /// 自动模式下的屏幕偏好；固定设备名始终拥有更高优先级。
    pub prefer_secondary_monitor: bool,
    pub preferred_width_px: u32,
    pub anchor: TaskbarAnchor,
    pub reserved_offset_px: i32,
    /// 与系统通知区域或左侧已有组件之间保留的安全间距。
    pub edge_gap_px: u32,
    /// 应用装配层声明是否采用任务栏子窗口；独立 `FloatingProbeWindow` 不读取此值。
    pub embed_in_taskbar: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            target_monitor_device: None,
            prefer_secondary_monitor: true,
            preferred_width_px: 320,
            anchor: TaskbarAnchor::Right,
            reserved_offset_px: 0,
            edge_gap_px: 8,
            embed_in_taskbar: true,
        }
    }
}

/// 进程是否运行在 Windows 上。
#[must_use]
pub const fn is_supported() -> bool {
    cfg!(windows)
}

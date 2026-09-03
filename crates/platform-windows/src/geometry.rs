//! 与 Win32 无关的任务栏坐标模型和布局算法。

use codex_taskbar_domain::layout::TaskbarAnchor;

use crate::{PlatformError, ProbeConfig};

/// 使用物理像素的屏幕矩形，右、下边界为排他边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PixelRect {
    #[must_use]
    pub const fn width(self) -> i32 {
        self.right - self.left
    }

    #[must_use]
    pub const fn height(self) -> i32 {
        self.bottom - self.top
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.width() > 0 && self.height() > 0
    }
}

/// 供布局器使用的任务栏几何快照。
///
/// `right_safe_boundary_x` 是通知区域（优先）或时钟的左边界。它未知时右锚定
/// 会失败，绝不以显示器右边界猜测，避免浮窗遮挡系统区域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskbarGeometry {
    pub taskbar_rect: PixelRect,
    pub monitor_rect: PixelRect,
    pub dpi: u32,
    pub right_safe_boundary_x: Option<i32>,
}

/// 根据任务栏的实际物理像素计算组件的屏幕位置。
///
/// 返回的矩形位于任务栏内；透明子窗口宿主会把它转换为 parent-client 坐标。`Right`
/// 一定使用 `right_safe_boundary_x`，缺失时返回错误而不是覆盖屏幕最右侧。
pub fn layout_probe(geometry: &TaskbarGeometry, config: &ProbeConfig) -> Result<PixelRect, PlatformError> {
    let taskbar = geometry.taskbar_rect;
    if !taskbar.is_valid() {
        return Err(PlatformError::InsufficientSpace);
    }

    let width = i32::try_from(config.preferred_width_px).map_err(|_| PlatformError::InsufficientSpace)?;
    let gap = i32::try_from(config.edge_gap_px).map_err(|_| PlatformError::InsufficientSpace)?;
    if width <= 0 || width > taskbar.width() {
        return Err(PlatformError::InsufficientSpace);
    }

    let x = match config.anchor {
        TaskbarAnchor::Left => taskbar.left.checked_add(gap).and_then(|v| v.checked_add(config.reserved_offset_px)),
        TaskbarAnchor::Right => geometry
            .right_safe_boundary_x
            .ok_or(PlatformError::MissingRightSafetyBoundary)?
            .checked_sub(gap)
            .and_then(|v| v.checked_sub(config.reserved_offset_px))
            .and_then(|v| v.checked_sub(width)),
    }
    .ok_or(PlatformError::InsufficientSpace)?;
    let right = x.checked_add(width).ok_or(PlatformError::InsufficientSpace)?;
    // 左右锚点均不能越出任务栏；这也会拒绝过大的 reserved_offset。
    if x < taskbar.left || right > taskbar.right {
        return Err(PlatformError::InsufficientSpace);
    }
    if config.anchor == TaskbarAnchor::Right {
        let safe_right = geometry
            .right_safe_boundary_x
            .ok_or(PlatformError::MissingRightSafetyBoundary)?
            .checked_sub(gap)
            .ok_or(PlatformError::InsufficientSpace)?;
        // 即使用户手工填写负偏移，也不能越过通知区域安全线。负值只能在仍处于安全区域时生效。
        if right > safe_right {
            return Err(PlatformError::InsufficientSpace);
        }
    }

    Ok(PixelRect { left: x, top: taskbar.top, right, bottom: taskbar.bottom })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(boundary: Option<i32>) -> TaskbarGeometry {
        TaskbarGeometry {
            taskbar_rect: PixelRect { left: 0, top: 1040, right: 1920, bottom: 1080 },
            monitor_rect: PixelRect { left: 0, top: 0, right: 1920, bottom: 1080 },
            dpi: 96,
            right_safe_boundary_x: boundary,
        }
    }

    #[test]
    fn right_anchor_stays_left_of_notification_area() {
        let config = ProbeConfig { preferred_width_px: 320, edge_gap_px: 8, ..ProbeConfig::default() };
        let rect = layout_probe(&geometry(Some(1600)), &config).unwrap();
        assert_eq!(rect, PixelRect { left: 1272, top: 1040, right: 1592, bottom: 1080 });
    }

    #[test]
    fn right_anchor_refuses_to_guess_screen_edge() {
        assert!(matches!(
            layout_probe(&geometry(None), &ProbeConfig::default()),
            Err(PlatformError::MissingRightSafetyBoundary)
        ));
    }

    #[test]
    fn left_anchor_applies_gap_and_reserved_offset() {
        let config = ProbeConfig {
            anchor: TaskbarAnchor::Left,
            preferred_width_px: 200,
            edge_gap_px: 8,
            reserved_offset_px: 24,
            ..ProbeConfig::default()
        };
        let rect = layout_probe(&geometry(None), &config).unwrap();
        assert_eq!(rect, PixelRect { left: 32, top: 1040, right: 232, bottom: 1080 });
    }

    #[test]
    fn offsets_that_escape_taskbar_are_rejected() {
        let config = ProbeConfig {
            anchor: TaskbarAnchor::Left,
            preferred_width_px: 300,
            reserved_offset_px: 1700,
            ..ProbeConfig::default()
        };
        assert!(matches!(layout_probe(&geometry(None), &config), Err(PlatformError::InsufficientSpace)));
    }

    #[test]
    fn negative_right_offset_cannot_cross_notification_boundary() {
        let config = ProbeConfig { reserved_offset_px: -24, ..ProbeConfig::default() };
        assert!(matches!(layout_probe(&geometry(Some(1600)), &config), Err(PlatformError::InsufficientSpace)));
    }
}

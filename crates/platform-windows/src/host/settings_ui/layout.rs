//! 设置窗口的纯布局模型。
//!
//! 这里不依赖 HWND、GDI 或用户配置，只根据窗口大小和 DPI 返回逻辑矩形。原生
//! 控件层负责把这些矩形映射为实际窗口位置，因此副屏、缩放和页面切换可以通过
//! 单元测试验证，不必依赖真实 Explorer 或桌面会话。

/// 非负像素大小。传入无效尺寸时，布局器会钳制为最小可用区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    #[must_use]
    pub(super) const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }
}

/// Windows 每英寸逻辑像素密度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dpi(u32);

impl Dpi {
    #[must_use]
    pub(super) const fn new(value: u32) -> Self {
        Self(value)
    }

    fn scale(self, logical_px: i32) -> i32 {
        ((logical_px as i64 * self.0.max(96) as i64 + 48) / 96) as i32
    }
}

/// 相对当前设置页内容区域的矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    #[must_use]
    pub(super) fn intersects(self, other: Self) -> bool {
        self.left < other.right && other.left < self.right && self.top < other.bottom && other.top < self.bottom
    }
}

/// 滚动后的内容偏移。只用于编辑页；主页始终使用默认值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ScrollOffset {
    pub x: i32,
    pub y: i32,
}

/// 设置主页的固定栏和三段页签布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SettingsHomeLayout {
    pub tabs: Vec<Rect>,
    pub content: Rect,
    pub action_bar: Rect,
}

/// 供应商编辑页的可滚动控件布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderEditorLayout {
    pub controls: Vec<Rect>,
    pub content_height: i32,
}

/// 计算三页分段导航与底部操作栏，避免两者在 100–200% DPI 下重叠。
#[must_use]
pub(super) fn settings_home_layout(size: Size, dpi: Dpi) -> SettingsHomeLayout {
    let width = size.width.max(dpi.scale(320));
    let height = size.height.max(dpi.scale(360));
    let margin = dpi.scale(16);
    let tab_top = dpi.scale(14);
    let tab_height = dpi.scale(34);
    let tab_gap = dpi.scale(4);
    let action_height = dpi.scale(54);
    let action_top = height.saturating_sub(margin).saturating_sub(action_height);
    let available = width.saturating_sub(margin * 2).saturating_sub(tab_gap * 2).max(3);
    let tab_width = (available / 3).max(1);
    let tabs = (0..3)
        .map(|index| {
            let left = margin + index * (tab_width + tab_gap);
            Rect { left, top: tab_top, right: left + tab_width, bottom: tab_top + tab_height }
        })
        .collect();
    SettingsHomeLayout {
        tabs,
        content: Rect {
            left: margin,
            top: tab_top + tab_height + dpi.scale(14),
            right: width - margin,
            bottom: action_top.saturating_sub(dpi.scale(10)),
        },
        action_bar: Rect { left: margin, top: action_top, right: width - margin, bottom: height - margin },
    }
}

/// 计算“连接信息”和“本地估算”两张编辑卡的单列栅格。
#[must_use]
pub(super) fn provider_editor_layout(size: Size, dpi: Dpi, offset: ScrollOffset) -> ProviderEditorLayout {
    let home = settings_home_layout(size, dpi);
    let card_gap = dpi.scale(14);
    let card_height = dpi.scale(268);
    let second_top = home.content.top + card_height + card_gap - offset.y;
    let first = Rect {
        left: home.content.left - offset.x,
        top: home.content.top - offset.y,
        right: home.content.right - offset.x,
        bottom: home.content.top + card_height - offset.y,
    };
    let second = Rect {
        left: home.content.left - offset.x,
        top: second_top,
        right: home.content.right - offset.x,
        bottom: second_top + card_height,
    };
    ProviderEditorLayout { controls: vec![first, second], content_height: second.bottom + offset.y + dpi.scale(16) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmented_tabs_and_action_bar_never_overlap_at_200_percent_dpi() {
        let layout = settings_home_layout(Size::new(1_200, 900), Dpi::new(192));
        assert_eq!(layout.tabs.len(), 3);
        assert!(layout.tabs.iter().all(|tab| !tab.intersects(layout.action_bar)));
    }

    #[test]
    fn provider_editor_layout_stays_inside_negative_coordinate_secondary_monitor_viewport() {
        let layout = provider_editor_layout(Size::new(900, 720), Dpi::new(144), ScrollOffset::default());
        assert!(layout.controls.iter().all(|rect| rect.left >= 0 && rect.right <= 900));
        assert!(layout.content_height > 720);
    }
}

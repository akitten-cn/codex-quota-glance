//! 可配置任务栏布局模型。

use serde::{Deserialize, Serialize};

/// 用户可以显示、隐藏和排序的原子组件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayItemKind {
    ActivityLight,
    /// 同一画布中的嵌套额度环：外环为周额度，内环为 5h；5h absent 时只绘制外环。
    QuotaRings,
    ResetCountdown,
    CurrentThreadTokens,
    TodayTokens,
    InputTokens,
    OutputTokens,
    CacheHitRate,
    DataFreshness,
}

/// 信息条在目标任务栏可用区域内的锚点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskbarAnchor {
    Left,
    Right,
}

/// 单个任务栏组件的布局配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayItemConfig {
    pub kind: DisplayItemKind,
    pub visible: bool,
    pub order: u16,
    pub min_width_px: u16,
    /// 空间不足时，较小值先被折叠。
    pub keep_priority: u8,
}

/// 返回稳定的可见组件顺序；相同 order 使用输入顺序，便于配置文件手工编辑。
#[must_use]
pub fn ordered_visible_items(items: &[DisplayItemConfig]) -> Vec<&DisplayItemConfig> {
    let mut indexed: Vec<_> = items.iter().enumerate().filter(|(_, item)| item.visible).collect();
    indexed.sort_by_key(|(index, item)| (item.order, *index));
    indexed.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_items_are_excluded_and_order_is_stable() {
        let items = [
            DisplayItemConfig {
                kind: DisplayItemKind::QuotaRings,
                visible: true,
                order: 2,
                min_width_px: 24,
                keep_priority: 10,
            },
            DisplayItemConfig {
                kind: DisplayItemKind::ActivityLight,
                visible: true,
                order: 1,
                min_width_px: 16,
                keep_priority: 20,
            },
            DisplayItemConfig {
                kind: DisplayItemKind::TodayTokens,
                visible: false,
                order: 0,
                min_width_px: 24,
                keep_priority: 10,
            },
        ];

        let ordered = ordered_visible_items(&items);
        assert_eq!(
            ordered.iter().map(|item| item.kind).collect::<Vec<_>>(),
            [DisplayItemKind::ActivityLight, DisplayItemKind::QuotaRings]
        );
    }
}

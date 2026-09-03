//! 设置窗口的轻量原生视觉令牌。
//!
//! 数值按 Windows `COLORREF` 的 BGR 内存顺序编码。它们只用于 GDI 自绘层，
//! 与账户、凭据或运行时数据完全无关。

/// 将 RGB 分量转换为 Windows `COLORREF` 的无符号数表示。
pub(super) const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

/// 淡灰蓝窗口底色，借鉴旧胶囊界面的低对比留白。
pub(super) const WINDOW_BACKGROUND: u32 = rgb(244, 247, 251);
/// 白色卡片层，用于组织供应商、同步、任务栏布局等互相独立的设置域。
pub(super) const CARD_BACKGROUND: u32 = rgb(255, 255, 255);
pub(super) const CARD_BORDER: u32 = rgb(222, 230, 240);
pub(super) const TAB_SELECTED: u32 = rgb(224, 236, 255);
pub(super) const TAB_IDLE: u32 = rgb(248, 250, 253);
pub(super) const TAB_PRESSED: u32 = rgb(205, 224, 251);
pub(super) const PRIMARY: u32 = rgb(47, 109, 246);
pub(super) const PRIMARY_PRESSED: u32 = rgb(35, 88, 212);
pub(super) const BUTTON_IDLE: u32 = rgb(255, 255, 255);
pub(super) const BUTTON_PRESSED: u32 = rgb(235, 240, 248);
pub(super) const TEXT_PRIMARY: u32 = rgb(27, 43, 66);
pub(super) const TEXT_ON_PRIMARY: u32 = rgb(255, 255, 255);

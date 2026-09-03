//! Windows Direct2D/DirectWrite 渲染器资源生命周期。
//!
//! 图元选择和动画决策位于 [`crate::render_model`]；本模块只拥有 COM 资源并在
//! `D2DERR_RECREATE_TARGET` 等设备丢失路径上释放可重建资源。它不会创建窗口、更改
//! Explorer 层级或嵌入任务栏。

use std::ffi::c_void;

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
        Graphics::{
            Direct2D::{
                Common::{
                    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
                    D2D1_FIGURE_END_CLOSED, D2D1_PIXEL_FORMAT,
                },
                D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
                D2D1_ROUNDED_RECT, D2D1CreateFactory, ID2D1Brush, ID2D1DCRenderTarget, ID2D1Factory, ID2D1Factory1,
                ID2D1RenderTarget, ID2D1SolidColorBrush,
            },
            DirectWrite::{
                DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
                DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory,
                IDWriteFactory, IDWriteFontCollection,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::{
                AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
                CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
            },
        },
        UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow},
    },
    core::{Error, Interface, w},
};

use codex_taskbar_domain::activity::LampColor;
use windows_numerics::{Matrix3x2, Vector2};

use crate::{
    host::{NativeDetailRowKind, NativeHostDetails, NativeMetricTone},
    render_model::{ActivityLampModel, Circle, DipRect, FluidQuotaModel, RenderModel, RingArc},
};

/// Direct2D/DirectWrite 工厂和可按 HWND 重建的 device resources。
///
/// 工厂在实例生命周期内复用；`target`、brush 和几何等与设备关联的对象必须在
/// [`Self::discard_device_resources`] 后重新创建。当前阶段刻意不拥有 HWND。
pub struct Direct2dRenderer {
    d2d_factory: ID2D1Factory1,
    dwrite_factory: IDWriteFactory,
    surface: Option<LayeredSurface>,
}

/// 32-bit premultiplied-alpha DIB 与绑定到它的 Direct2D DC render target。
/// 每帧通过 UpdateLayeredWindow 提交，使未绘制像素保持真正的 0 alpha。
struct LayeredSurface {
    dc_target: Option<ID2D1DCRenderTarget>,
    render_target: Option<ID2D1RenderTarget>,
    memory_dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    width_px: u32,
    height_px: u32,
}

/// 详情卡按较宽的 960×660 DIP 设计。小工作区会等比缩放，正常副屏不再把
/// 账户、额度与今日消耗挤在一张窄卡里。
const DETAILS_LAYOUT_SIZE: (f32, f32) = (960.0, 660.0);
/// 自动用量摘要卡。六项“本次”指标横向排布，底部为朝向任务栏的小尖角。
const TOKEN_STRIP_LAYOUT_SIZE: (f32, f32) = (620.0, 112.0);

/// 详情卡顶部的可操作入口。枚举保持平台内部可见，窗口消息层只需要把命中结果
/// 映射成平台无关的 `NativeHostEvent`，不解释按钮的业务实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailsAction {
    Refresh,
    OpenSettings,
}

impl Direct2dRenderer {
    /// 创建可长期复用的 Direct2D 与 DirectWrite 工厂，不创建窗口或渲染目标。
    pub fn new() -> Result<Self, Error> {
        // SAFETY: 参数为 None 时 windows-rs 传递空的 factory options；返回的 COM 接口由 Rust 引用计数管理。
        let d2d_factory = unsafe {
            D2D1CreateFactory::<ID2D1Factory1>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None::<*const D2D1_FACTORY_OPTIONS>)?
        };
        // SAFETY: 创建共享 DirectWrite 工厂不接收 Rust 指针，接口生命周期由返回值管理。
        let dwrite_factory = unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(Self { d2d_factory, dwrite_factory, surface: None })
    }

    /// 为已有的本进程 HWND 创建或重建渲染目标。调用方负责在 UI 线程串行调用。
    pub fn recreate_device_resources(
        &mut self,
        _hwnd: HWND,
        width_px: u32,
        height_px: u32,
        dpi: f32,
    ) -> Result<(), Error> {
        self.discard_device_resources();
        let target_properties = D2D1_RENDER_TARGET_PROPERTIES {
            // 最终画面本来就要落到 CPU 侧 32-bit DIB，再由 UpdateLayeredWindow
            // 提交。DEFAULT 会优先创建硬件路径并加载 D3D/GPU 驱动，给这个最多
            // 20 FPS 的小型组件带来远高于绘制本身的固定内存和线程成本。
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi,
            dpiY: dpi,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: Default::default(),
        };
        let factory: ID2D1Factory = self.d2d_factory.cast()?;
        let mut surface = LayeredSurface::new(width_px, height_px)?;
        // SAFETY: 属性在调用期间有效；返回 COM 接口由 surface 持有。
        let dc_target = unsafe { factory.CreateDCRenderTarget(&target_properties)? };
        let bounds = RECT { left: 0, top: 0, right: width_px as i32, bottom: height_px as i32 };
        // SAFETY: memory_dc 已选入与 bounds 同尺寸的 32-bit DIB，二者在 surface 生命周期内有效。
        unsafe { dc_target.BindDC(surface.memory_dc, &bounds)? };
        let render_target: ID2D1RenderTarget = dc_target.cast()?;
        surface.dc_target = Some(dc_target);
        surface.render_target = Some(render_target);
        self.surface = Some(surface);
        Ok(())
    }

    /// 设备丢失时调用；下一次 paint 通过 `recreate_device_resources` 延迟重建。
    pub fn discard_device_resources(&mut self) {
        self.surface = None;
    }

    /// `EndDraw` 报告设备丢失时的语义化入口。
    pub fn on_device_lost(&mut self) {
        self.discard_device_resources();
    }

    /// 是否已具备可绘制的 HWND render target。
    #[must_use]
    pub fn has_device_resources(&self) -> bool {
        self.surface.is_some()
    }

    /// 暴露 Direct2D/DirectWrite 工厂给测试或未来布局扩展层。
    #[must_use]
    pub fn dwrite_factory(&self) -> &IDWriteFactory {
        &self.dwrite_factory
    }

    /// 直接把 DPI 无关的绘制模型提交给当前 HWND render target。
    pub fn draw(
        &self,
        hwnd: HWND,
        model: &RenderModel,
        details: &NativeHostDetails,
        _dirty: Option<DipRect>,
    ) -> Result<(), Error> {
        let surface = self.surface.as_ref().ok_or_else(Error::from_thread)?;
        let target = surface.render_target.as_ref().ok_or_else(Error::from_thread)?;
        // SAFETY: target 在本渲染器生命周期内有效，所有 Direct2D 调用均在宿主 UI 线程串行执行。
        unsafe {
            target.BeginDraw();
            let draw_result = (|| -> Result<(), Error> {
                target.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
                if model.show_quota {
                    draw_fluid_quota(
                        self.dwrite_factory(),
                        target,
                        model.fluid,
                        model.taskbar_background_opacity,
                        details,
                    )?;
                }
                Ok(())
            })();
            let end_result = target.EndDraw(None, None);
            draw_result?;
            end_result?;
        }
        surface.present(hwnd)?;
        Ok(())
    }

    /// 绘制任务栏上方的旧版详情卡片。该窗口仍使用逐像素 alpha，但卡片本身
    /// 是完整的圆角信息面板，而不是系统 MessageBox 或胶囊形摘要。
    pub(crate) fn draw_details_card(
        &self,
        hwnd: HWND,
        details: &NativeHostDetails,
        hovered_trend_index: Option<usize>,
        hovered_action: Option<DetailsAction>,
    ) -> Result<(), Error> {
        let surface = self.surface.as_ref().ok_or_else(Error::from_thread)?;
        let target = surface.render_target.as_ref().ok_or_else(Error::from_thread)?;
        let size = unsafe { target.GetSize() };
        unsafe {
            target.BeginDraw();
            let transform = fit_layout_transform((size.width, size.height), DETAILS_LAYOUT_SIZE);
            target.SetTransform(&transform);
            let draw_result = {
                target.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
                draw_details_card_content(
                    self.dwrite_factory(),
                    target,
                    DETAILS_LAYOUT_SIZE,
                    details,
                    hovered_trend_index,
                    hovered_action,
                )
            };
            target.SetTransform(&identity_transform());
            let end_result = target.EndDraw(None, None);
            draw_result?;
            end_result?;
        }
        surface.present(hwnd)?;
        Ok(())
    }

    /// 绘制从任务栏上方升起的 Token 快览滑块。
    pub fn draw_token_strip(&self, hwnd: HWND, details: &NativeHostDetails) -> Result<(), Error> {
        let surface = self.surface.as_ref().ok_or_else(Error::from_thread)?;
        let target = surface.render_target.as_ref().ok_or_else(Error::from_thread)?;
        let size = unsafe { target.GetSize() };
        unsafe {
            target.BeginDraw();
            let transform = fit_layout_transform((size.width, size.height), TOKEN_STRIP_LAYOUT_SIZE);
            target.SetTransform(&transform);
            let draw_result = {
                target.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
                draw_token_strip_content(self.dwrite_factory(), target, TOKEN_STRIP_LAYOUT_SIZE, details)
            };
            target.SetTransform(&identity_transform());
            let end_result = target.EndDraw(None, None);
            draw_result?;
            end_result?;
        }
        surface.present(hwnd)?;
        Ok(())
    }
}

/// 把固定 DIP 设计稿等比放入当前 surface。高 DPI 小工作区会先由窗口层缩小
/// 像素尺寸，再由这里整体缩放文字与图元，因此不会出现“窗口缩小但内容仍被裁掉”。
fn fit_layout_transform(surface: (f32, f32), layout: (f32, f32)) -> Matrix3x2 {
    let scale = (surface.0 / layout.0).min(surface.1 / layout.1).clamp(0.01, 1.0);
    let offset_x = ((surface.0 - layout.0 * scale) / 2.0).max(0.0);
    let offset_y = ((surface.1 - layout.1 * scale) / 2.0).max(0.0);
    Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: offset_x, M32: offset_y }
}

const fn identity_transform() -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 }
}

fn draw_details_card_content(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    size: (f32, f32),
    details: &NativeHostDetails,
    hovered_trend_index: Option<usize>,
    hovered_action: Option<DetailsAction>,
) -> Result<(), Error> {
    let card = D2D_RECT_F { left: 10.0, top: 8.0, right: size.0 - 10.0, bottom: size.1 - 12.0 };
    // 详情、任务栏和自动上浮卡使用同一深海青紫基调；不再保留旧版浅色
    // 信息表，从而避免三个窗口各自像不同软件。
    for (inset, alpha, width) in [(0.0, 0.32, 10.0), (2.0, 0.24, 5.0), (4.0, 0.18, 2.0)] {
        let shadow = unsafe { create_brush(target, color(0.0, 0.0, 0.0, alpha))? };
        let rounded = rounded_rect(
            D2D_RECT_F {
                left: card.left + inset,
                top: card.top + inset + 2.0,
                right: card.right - inset,
                bottom: card.bottom - inset + 2.0,
            },
            12.0,
        );
        unsafe { target.DrawRoundedRectangle(&rounded, &shadow, width, None) };
    }

    let background = unsafe { create_brush(target, color(0.035, 0.047, 0.090, 0.99))? };
    let border = unsafe { create_brush(target, color(0.36, 0.55, 0.90, 0.34))? };
    let title = unsafe { create_brush(target, color(0.94, 0.97, 1.0, 1.0))? };
    let primary = unsafe { create_brush(target, color(0.90, 0.94, 1.0, 0.98))? };
    let muted = unsafe { create_brush(target, color(0.60, 0.69, 0.83, 0.94))? };
    let accent = unsafe { create_brush(target, color(0.35, 0.92, 0.93, 1.0))? };
    let divider = unsafe { create_brush(target, color(0.46, 0.60, 0.94, 0.20))? };
    let badge_bg = unsafe { create_brush(target, color(0.25, 0.18, 0.52, 0.86))? };

    let panel = rounded_rect(card, 10.0);
    unsafe {
        target.FillRoundedRectangle(&panel, &background);
        target.DrawRoundedRectangle(&panel, &border, 1.0, None);
    }

    let left = card.left + 16.0;
    let right = card.right - 16.0;
    let refresh_rect = details_action_bounds(DetailsAction::Refresh);
    draw_text(
        factory,
        target,
        &title,
        rect(left, card.top + 10.0, refresh_rect.left - 10.0, card.top + 36.0),
        &details.title,
        18.0,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let badge_rect = rect(right - 82.0, card.top + 9.0, right, card.top + 32.0);
    unsafe { target.FillRoundedRectangle(&rounded_rect(badge_rect, 10.0), &badge_bg) };
    draw_text(
        factory,
        target,
        &accent,
        badge_rect,
        &details.badge,
        12.25,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    )?;
    draw_details_action_buttons(factory, target, hovered_action, &primary, &muted)?;

    // 活动状态已由任务栏的前景浪色/速度表达，详情卡不再重复一颗状态灯。
    draw_text(
        factory,
        target,
        &muted,
        rect(left, card.top + 34.0, right - 110.0, card.top + 51.0),
        &format!("{} · {}", details.status, details.updated),
        11.5,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;

    unsafe {
        target.DrawLine(Vector2::new(left, card.top + 55.0), Vector2::new(right, card.top + 55.0), &divider, 1.0, None)
    };
    if !details.metric_cards.is_empty() {
        draw_dashboard_details(
            factory,
            target,
            card,
            details,
            hovered_trend_index,
            &primary,
            &muted,
            &divider,
            &accent,
        )?;
        return Ok(());
    }
    let columns_top = card.top + 65.0;
    let columns_bottom = card.bottom - 32.0;
    let gap = 24.0;
    let column_width = (right - left - gap) / 2.0;
    draw_detail_column(
        factory,
        target,
        rect(left, columns_top, left + column_width, columns_bottom),
        &details.primary_heading,
        &details.primary_rows,
        &muted,
        &primary,
    )?;
    draw_detail_column(
        factory,
        target,
        rect(left + column_width + gap, columns_top, right, columns_bottom),
        &details.secondary_heading,
        &details.secondary_rows,
        &muted,
        &primary,
    )?;
    unsafe {
        target.DrawLine(
            Vector2::new(left + column_width + gap / 2.0, columns_top + 4.0),
            Vector2::new(left + column_width + gap / 2.0, columns_bottom - 4.0),
            &divider,
            1.0,
            None,
        )
    };
    draw_text(
        factory,
        target,
        &muted,
        rect(left, card.bottom - 29.0, right, card.bottom - 8.0),
        &details.footer,
        9.25,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    )?;
    Ok(())
}

/// 绘制详情卡的两个轻量操作入口。按钮位于固定 880×590 DIP 设计坐标内，
/// 因而和鼠标命中使用完全相同的几何，不会在高 DPI 或小工作区缩放后错位。
fn draw_details_action_buttons(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    hovered_action: Option<DetailsAction>,
    primary: &ID2D1SolidColorBrush,
    muted: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    for (action, copy) in [(DetailsAction::Refresh, "↻  刷新"), (DetailsAction::OpenSettings, "⚙  设置")] {
        let bounds = details_action_bounds(action);
        let hovered = hovered_action == Some(action);
        // 操作按钮同样是深色半透明层；此前的白色按钮来自旧仪表盘样式，
        // 会让官方详情卡看起来像另一个应用。
        let background = unsafe {
            create_brush(target, if hovered { color(0.09, 0.27, 0.36, 0.98) } else { color(0.07, 0.12, 0.22, 0.96) })?
        };
        let border = unsafe {
            create_brush(target, if hovered { color(0.25, 0.90, 0.86, 0.64) } else { color(0.31, 0.48, 0.76, 0.42) })?
        };
        unsafe {
            target.FillRoundedRectangle(&rounded_rect(bounds, 7.0), &background);
            target.DrawRoundedRectangle(&rounded_rect(bounds, 7.0), &border, if hovered { 1.0 } else { 0.8 }, None);
        }
        draw_text(
            factory,
            target,
            if hovered { primary } else { muted },
            bounds,
            copy,
            10.75,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
    }
    Ok(())
}

fn details_action_bounds(action: DetailsAction) -> D2D_RECT_F {
    let card_right = DETAILS_LAYOUT_SIZE.0 - 10.0;
    let content_right = card_right - 16.0;
    let badge_left = content_right - 82.0;
    let settings_right = badge_left - 8.0;
    let settings_left = settings_right - 70.0;
    let refresh_right = settings_left - 7.0;
    let refresh_left = refresh_right - 70.0;
    let (left, right) = match action {
        DetailsAction::Refresh => (refresh_left, refresh_right),
        DetailsAction::OpenSettings => (settings_left, settings_right),
    };
    rect(left, 17.0, right, 41.0)
}

#[allow(clippy::too_many_arguments)]
fn draw_dashboard_details(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    card: D2D_RECT_F,
    details: &NativeHostDetails,
    hovered_trend_index: Option<usize>,
    primary: &ID2D1SolidColorBrush,
    muted: &ID2D1SolidColorBrush,
    divider: &ID2D1SolidColorBrush,
    accent: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    if details.compact_primary_column {
        return draw_compact_official_details(
            factory,
            target,
            card,
            details,
            hovered_trend_index,
            primary,
            muted,
            divider,
            accent,
        );
    }
    let left = card.left + 16.0;
    let right = card.right - 16.0;
    let hero_top = card.top + 68.0;
    let hero_bottom = hero_top + 84.0;
    let hero_width = 194.0;
    let hero_rect = rect(left, hero_top, left + hero_width, hero_bottom);
    let hero_bg = unsafe { create_brush(target, color(0.91, 0.96, 0.93, 0.92))? };
    let hero_border = unsafe { create_brush(target, color(0.20, 0.48, 0.33, 0.20))? };
    let show_hero = details.metric_cards.len() != 2;
    if show_hero {
        unsafe {
            target.FillRoundedRectangle(&rounded_rect(hero_rect, 10.0), &hero_bg);
            target.DrawRoundedRectangle(&rounded_rect(hero_rect, 10.0), &hero_border, 1.0, None);
        }
        draw_text(
            factory,
            target,
            muted,
            rect(hero_rect.left + 12.0, hero_rect.top + 7.0, hero_rect.right - 10.0, hero_rect.top + 25.0),
            &details.hero_label,
            11.5,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            accent,
            rect(hero_rect.left + 12.0, hero_rect.top + 23.0, hero_rect.right - 10.0, hero_rect.top + 57.0),
            &details.hero_value,
            29.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            muted,
            rect(hero_rect.left + 12.0, hero_rect.top + 56.0, hero_rect.right - 10.0, hero_rect.bottom - 4.0),
            &details.hero_hint,
            10.75,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
    }

    let metrics_left = if show_hero { hero_rect.right + 12.0 } else { left };
    let metric_gap = 10.0;
    let card_count = details.metric_cards.len().clamp(1, 2);
    let metric_width = (right - metrics_left - metric_gap * (card_count.saturating_sub(1) as f32)) / card_count as f32;
    for (index, metric) in details.metric_cards.iter().take(2).enumerate() {
        let metric_left = metrics_left + index as f32 * (metric_width + metric_gap);
        let metric_rect = rect(metric_left, hero_top, metric_left + metric_width, hero_bottom);
        let (background_color, border_color, value_color) = match metric.tone {
            NativeMetricTone::Positive => {
                (color(0.075, 0.18, 0.22, 0.96), color(0.16, 0.82, 0.76, 0.42), color(0.33, 0.94, 0.86, 1.0))
            }
            NativeMetricTone::Warning => {
                (color(0.22, 0.15, 0.06, 0.96), color(0.95, 0.62, 0.12, 0.46), color(1.0, 0.76, 0.31, 1.0))
            }
            NativeMetricTone::Critical => {
                (color(0.25, 0.08, 0.13, 0.96), color(0.98, 0.33, 0.48, 0.48), color(1.0, 0.49, 0.61, 1.0))
            }
            NativeMetricTone::Neutral => {
                (color(0.08, 0.10, 0.17, 0.96), color(0.44, 0.54, 0.73, 0.30), color(0.72, 0.79, 0.91, 1.0))
            }
        };
        let background = unsafe { create_brush(target, background_color)? };
        let border = unsafe { create_brush(target, border_color)? };
        let value = unsafe { create_brush(target, value_color)? };
        unsafe {
            target.FillRoundedRectangle(&rounded_rect(metric_rect, 10.0), &background);
            target.DrawRoundedRectangle(&rounded_rect(metric_rect, 10.0), &border, 1.0, None);
        }
        draw_text(
            factory,
            target,
            muted,
            rect(metric_rect.left + 12.0, metric_rect.top + 7.0, metric_rect.right - 10.0, metric_rect.top + 25.0),
            &metric.label,
            11.5,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            &value,
            rect(metric_rect.left + 12.0, metric_rect.top + 24.0, metric_rect.right - 10.0, metric_rect.top + 51.0),
            &metric.value,
            20.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            muted,
            rect(metric_rect.left + 12.0, metric_rect.top + 52.0, metric_rect.right - 10.0, metric_rect.bottom - 15.0),
            &metric.detail,
            10.75,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        if let Some(percent) = metric.progress_percent {
            let track = unsafe { create_brush(target, color(0.12, 0.20, 0.25, 0.10))? };
            let progress = unsafe { create_brush(target, value_color)? };
            let track_rect = rect(
                metric_rect.left + 12.0,
                metric_rect.bottom - 9.0,
                metric_rect.right - 12.0,
                metric_rect.bottom - 5.0,
            );
            let fill_rect = rect(
                track_rect.left,
                track_rect.top,
                track_rect.left + (track_rect.right - track_rect.left) * f32::from(percent) / 100.0,
                track_rect.bottom,
            );
            unsafe {
                target.FillRoundedRectangle(&rounded_rect(track_rect, 2.0), &track);
                if fill_rect.right > fill_rect.left {
                    target.FillRoundedRectangle(&rounded_rect(fill_rect, 2.0), &progress);
                }
            }
        }
    }

    let health_top = card.bottom - 57.0;
    let mut columns_top = hero_bottom + 11.0;
    if !details.chart_segments.is_empty() {
        let chart_top = hero_bottom + 8.0;
        let chart_bottom = chart_top + 54.0;
        draw_token_chart(
            factory,
            target,
            rect(left, chart_top, right, chart_bottom),
            &details.chart_segments,
            &details.chart_title,
            primary,
            muted,
        )?;
        columns_top = chart_bottom + 10.0;
    }
    let columns_bottom = health_top - 8.0;
    let gap = 24.0;
    let column_width = (right - left - gap) / 2.0;
    draw_detail_column(
        factory,
        target,
        rect(left, columns_top, left + column_width, columns_bottom),
        &details.primary_heading,
        &details.primary_rows,
        muted,
        primary,
    )?;
    draw_detail_column(
        factory,
        target,
        rect(left + column_width + gap, columns_top, right, columns_bottom),
        &details.secondary_heading,
        &details.secondary_rows,
        muted,
        primary,
    )?;
    unsafe {
        target.DrawLine(
            Vector2::new(left + column_width + gap / 2.0, columns_top + 4.0),
            Vector2::new(left + column_width + gap / 2.0, columns_bottom - 4.0),
            divider,
            1.0,
            None,
        )
    };

    let health_rows = details.health_rows.iter().take(4).collect::<Vec<_>>();
    if !health_rows.is_empty() {
        let health_bg = unsafe { create_brush(target, color(0.92, 0.94, 0.97, 0.72))? };
        let health_rect = rect(left, health_top, right, health_top + 24.0);
        unsafe { target.FillRoundedRectangle(&rounded_rect(health_rect, 7.0), &health_bg) };
        let width = (right - left - 12.0) / health_rows.len() as f32;
        for (index, row) in health_rows.iter().enumerate() {
            let item_left = left + 6.0 + index as f32 * width;
            draw_text(
                factory,
                target,
                primary,
                rect(item_left, health_top + 1.0, item_left + width - 5.0, health_top + 22.0),
                &format!("{}  {}", row.label, row.value),
                10.75,
                DWRITE_FONT_WEIGHT_MEDIUM,
                DWRITE_TEXT_ALIGNMENT_CENTER,
            )?;
        }
    }
    draw_text(
        factory,
        target,
        muted,
        rect(left, card.bottom - 29.0, right, card.bottom - 8.0),
        &details.footer,
        10.5,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    )?;
    Ok(())
}

/// 官方账户详情使用“窄额度栏 + 纵向 Token 仪表盘”。账户和额度是必要背景，
/// 但不应与每日高频查看的 Token 数据平均争抢宽度。右侧按“总量、明细、趋势、
/// 构成”的顺序阅读，避免明细与趋势横向挤压后两边都难以辨认。
#[allow(clippy::too_many_arguments)]
fn draw_compact_official_details(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    card: D2D_RECT_F,
    details: &NativeHostDetails,
    hovered_trend_index: Option<usize>,
    primary: &ID2D1SolidColorBrush,
    muted: &ID2D1SolidColorBrush,
    divider: &ID2D1SolidColorBrush,
    accent: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let left = card.left + 16.0;
    let right = card.right - 16.0;
    let body_top = card.top + 68.0;
    let health_top = card.bottom - 57.0;
    let body_bottom = health_top - 8.0;
    let left_width = compact_official_left_width(right - left);
    let gap = 18.0;
    let divider_x = left + left_width + gap / 2.0;
    let secondary_left = left + left_width + gap;

    draw_text(
        factory,
        target,
        primary,
        rect(left, body_top, left + left_width, body_top + 22.0),
        &details.primary_heading,
        12.75,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;

    let metric_gap = 7.0;
    let metric_height = 70.0;
    let metrics_top = body_top + 28.0;
    for (index, metric) in details.metric_cards.iter().take(2).enumerate() {
        let top = metrics_top + index as f32 * (metric_height + metric_gap);
        let metric_rect = rect(left, top, left + left_width, top + metric_height);
        // 额度是同一套叠浪视觉的延展，不再把低额度渲染成突兀的红/棕色实心
        // 告警块。风险仅通过数值色与进度长度表达，背景始终保持深海玻璃质感。
        let value_color = match metric.tone {
            NativeMetricTone::Positive => color(0.32, 0.94, 0.86, 1.0),
            NativeMetricTone::Warning => color(1.0, 0.76, 0.34, 1.0),
            NativeMetricTone::Critical => color(1.0, 0.49, 0.64, 1.0),
            NativeMetricTone::Neutral => color(0.67, 0.78, 1.0, 1.0),
        };
        let background_color = color(0.045, 0.070, 0.125, 0.96);
        let border_color = color(0.34, 0.50, 0.78, 0.38);
        let background = unsafe { create_brush(target, background_color)? };
        let border = unsafe { create_brush(target, border_color)? };
        let value = unsafe { create_brush(target, value_color)? };
        unsafe {
            target.FillRoundedRectangle(&rounded_rect(metric_rect, 10.0), &background);
            target.DrawRoundedRectangle(&rounded_rect(metric_rect, 10.0), &border, 1.0, None);
        }
        if let Some(percent) = metric.progress_percent {
            let wave = unsafe {
                create_brush(
                    target,
                    if index == 0 { color(0.42, 0.28, 0.98, 0.35) } else { color(0.14, 0.71, 0.96, 0.33) },
                )?
            };
            draw_wave_fill(
                target,
                rect(metric_rect.left + 1.0, metric_rect.top + 1.0, metric_rect.right - 1.0, metric_rect.bottom - 1.0),
                f32::from(percent),
                index as f32 * 0.23,
                2.0,
                1.25,
                &wave,
            )?;
        }
        draw_text(
            factory,
            target,
            muted,
            rect(metric_rect.left + 10.0, metric_rect.top + 4.0, metric_rect.right - 9.0, metric_rect.top + 21.0),
            &metric.label,
            10.75,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            &value,
            rect(metric_rect.left + 10.0, metric_rect.top + 19.0, metric_rect.right - 9.0, metric_rect.top + 44.0),
            &metric.value,
            17.5,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            muted,
            rect(metric_rect.left + 10.0, metric_rect.top + 43.0, metric_rect.right - 9.0, metric_rect.bottom - 10.0),
            &metric.detail,
            9.75,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        if let Some(percent) = metric.progress_percent {
            let track = unsafe { create_brush(target, color(0.01, 0.03, 0.06, 0.54))? };
            let progress = unsafe { create_brush(target, value_color)? };
            let track_rect = rect(
                metric_rect.left + 10.0,
                metric_rect.bottom - 8.0,
                metric_rect.right - 10.0,
                metric_rect.bottom - 4.0,
            );
            let fill_rect = rect(
                track_rect.left,
                track_rect.top,
                track_rect.left + (track_rect.right - track_rect.left) * f32::from(percent) / 100.0,
                track_rect.bottom,
            );
            unsafe {
                target.FillRoundedRectangle(&rounded_rect(track_rect, 2.0), &track);
                if fill_rect.right > fill_rect.left {
                    target.FillRoundedRectangle(&rounded_rect(fill_rect, 2.0), &progress);
                }
            }
        }
    }

    let metrics_bottom = metrics_top
        + details.metric_cards.len().min(2) as f32 * metric_height
        + details.metric_cards.len().min(2).saturating_sub(1) as f32 * metric_gap;
    draw_compact_account_column(
        factory,
        target,
        rect(left, metrics_bottom + 9.0, left + left_width, body_bottom),
        "账户信息",
        &details.primary_rows,
        muted,
        primary,
    )?;

    unsafe {
        target.DrawLine(
            Vector2::new(divider_x, body_top + 2.0),
            Vector2::new(divider_x, body_bottom - 2.0),
            divider,
            1.0,
            None,
        )
    };

    draw_text(
        factory,
        target,
        primary,
        rect(secondary_left, body_top, right, body_top + 22.0),
        &details.secondary_heading,
        13.25,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let hero_top = body_top + 28.0;
    let hero_bottom = hero_top + 82.0;
    let hero_rect = rect(secondary_left, hero_top, right, hero_bottom);
    let hero_bg = unsafe { create_brush(target, color(0.07, 0.11, 0.20, 0.97))? };
    let hero_border = unsafe { create_brush(target, color(0.25, 0.72, 0.93, 0.34))? };
    unsafe {
        target.FillRoundedRectangle(&rounded_rect(hero_rect, 11.0), &hero_bg);
        target.DrawRoundedRectangle(&rounded_rect(hero_rect, 11.0), &hero_border, 1.0, None);
    }
    draw_text(
        factory,
        target,
        muted,
        rect(hero_rect.left + 14.0, hero_rect.top + 7.0, hero_rect.right - 190.0, hero_rect.top + 27.0),
        &details.hero_label,
        11.75,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    draw_text(
        factory,
        target,
        accent,
        rect(hero_rect.left + 14.0, hero_rect.top + 25.0, hero_rect.right - 190.0, hero_rect.bottom - 8.0),
        &details.hero_value,
        32.5,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let hint_rect =
        rect(hero_rect.right - 178.0, hero_rect.top + 12.0, hero_rect.right - 12.0, hero_rect.bottom - 12.0);
    let hint_bg = unsafe { create_brush(target, color(0.26, 0.17, 0.55, 0.52))? };
    unsafe { target.FillRoundedRectangle(&rounded_rect(hint_rect, 10.0), &hint_bg) };
    draw_text(
        factory,
        target,
        primary,
        hint_rect,
        &details.hero_hint,
        11.25,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    )?;

    let trend_visible = details.trend_points.len() >= 2;
    let usage_panel_top = hero_bottom + 8.0;
    let usage_panel = rect(secondary_left, usage_panel_top, right, body_bottom);
    let usage_bg = unsafe { create_brush(target, color(0.055, 0.076, 0.135, 0.94))? };
    let usage_border = unsafe { create_brush(target, color(0.34, 0.56, 0.92, 0.30))? };
    unsafe {
        target.FillRoundedRectangle(&rounded_rect(usage_panel, 10.0), &usage_bg);
        target.DrawRoundedRectangle(&rounded_rect(usage_panel, 10.0), &usage_border, 1.0, None);
    }
    let content_left = secondary_left + 12.0;
    let content_right = right - 12.0;
    let content_top = usage_panel_top + 8.0;
    let content_bottom = body_bottom - 8.0;
    let chart_height = 50.0;
    let chart_top = content_bottom - chart_height;
    let detail_bottom = if trend_visible { content_top + 132.0 } else { chart_top - 8.0 };
    let (task_rows, account_rows) = split_official_detail_rows(&details.secondary_rows);
    if account_rows.is_empty() {
        draw_dense_detail_column(
            factory,
            target,
            rect(content_left, content_top, content_right, detail_bottom),
            &details.secondary_heading,
            task_rows,
            muted,
            primary,
        )?;
    } else {
        let details_gap = 12.0;
        let details_width = (content_right - content_left - details_gap) / 2.0;
        draw_dense_detail_column(
            factory,
            target,
            rect(content_left, content_top, content_left + details_width, detail_bottom),
            &details.secondary_heading,
            task_rows,
            muted,
            primary,
        )?;
        draw_dense_detail_column(
            factory,
            target,
            rect(content_left + details_width + details_gap, content_top, content_right, detail_bottom),
            "账户汇总",
            account_rows,
            muted,
            primary,
        )?;
        unsafe {
            target.DrawLine(
                Vector2::new(content_left + details_width + details_gap / 2.0, content_top + 3.0),
                Vector2::new(content_left + details_width + details_gap / 2.0, detail_bottom - 3.0),
                divider,
                0.8,
                None,
            )
        };
    }

    if trend_visible {
        draw_trend_chart(
            factory,
            target,
            rect(content_left, detail_bottom + 8.0, content_right, chart_top - 8.0),
            &details.trend_points,
            &details.trend_title,
            hovered_trend_index,
            primary,
            muted,
        )?;
    }
    if !details.chart_segments.is_empty() {
        draw_token_chart(
            factory,
            target,
            rect(content_left, chart_top, content_right, content_bottom),
            &details.chart_segments,
            &details.chart_title,
            primary,
            muted,
        )?;
    }

    let health_rows = details.health_rows.iter().take(4).collect::<Vec<_>>();
    if !health_rows.is_empty() {
        let health_bg = unsafe { create_brush(target, color(0.055, 0.095, 0.17, 0.94))? };
        let health_rect = rect(left, health_top, right, health_top + 24.0);
        unsafe { target.FillRoundedRectangle(&rounded_rect(health_rect, 7.0), &health_bg) };
        let width = (right - left - 12.0) / health_rows.len() as f32;
        for (index, row) in health_rows.iter().enumerate() {
            let item_left = left + 6.0 + index as f32 * width;
            draw_text(
                factory,
                target,
                primary,
                rect(item_left, health_top + 1.0, item_left + width - 5.0, health_top + 22.0),
                &format!("{}  {}", row.label, row.value),
                10.75,
                DWRITE_FONT_WEIGHT_MEDIUM,
                DWRITE_TEXT_ALIGNMENT_CENTER,
            )?;
        }
    }
    draw_text(
        factory,
        target,
        muted,
        rect(left, card.bottom - 29.0, right, card.bottom - 8.0),
        &details.footer,
        10.5,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    )?;
    Ok(())
}

/// 详情卡以 880 DIP 为基准绘制；这里仍保留范围限制，防止后续调整卡片尺寸时
/// 左栏重新膨胀。当前基准下左栏约占内容区四分之一。
fn compact_official_left_width(content_width: f32) -> f32 {
    (content_width * 0.26).clamp(210.0, 226.0)
}

/// 官方详情的 secondary_rows 通过 Section 行区分本机任务和账户活动。
/// 保持切片视图可避免渲染帧中复制字符串，也不会让平台层解释具体业务字段。
fn split_official_detail_rows(
    rows: &[crate::host::NativeDetailRow],
) -> (&[crate::host::NativeDetailRow], &[crate::host::NativeDetailRow]) {
    rows.iter()
        .position(|row| row.kind == NativeDetailRowKind::Section)
        .map_or((rows, &[]), |index| (&rows[..index], &rows[index + 1..]))
}

/// 左栏宽度较窄，账户字段改为“标签在上、值在下”，避免邮箱、认证方式和消费
/// 上限被两个横向列同时挤压。行高根据字段数量自适应，最多保持 36 DIP。
#[allow(clippy::too_many_arguments)]
fn draw_compact_account_column(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    heading: &str,
    rows: &[crate::host::NativeDetailRow],
    muted: &ID2D1SolidColorBrush,
    primary: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    draw_text(
        factory,
        target,
        primary,
        rect(bounds.left, bounds.top, bounds.right, bounds.top + 20.0),
        heading,
        12.25,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let row_top = bounds.top + 22.0;
    let available = (bounds.bottom - row_top).max(1.0);
    let row_height = (available / rows.len().max(1) as f32).min(36.0);
    for (index, row) in rows.iter().enumerate() {
        let top = row_top + index as f32 * row_height;
        if top + row_height > bounds.bottom + 0.5 {
            break;
        }
        draw_text(
            factory,
            target,
            muted,
            rect(bounds.left, top, bounds.right, top + row_height * 0.43),
            &row.label,
            10.25,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            primary,
            rect(bounds.left, top + row_height * 0.38, bounds.right, top + row_height),
            &row.value,
            11.75,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
    }
    Ok(())
}

/// 右侧明细使用更接近监控面板的紧凑键值列表。它只压缩行间距，不缩小到任务栏
/// 字号，确保在 125%–200% DPI 下仍有清晰的标签/数值层级。
#[allow(clippy::too_many_arguments)]
fn draw_dense_detail_column(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    heading: &str,
    rows: &[crate::host::NativeDetailRow],
    muted: &ID2D1SolidColorBrush,
    primary: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let heading_marker = unsafe { create_brush(target, color(0.20, 0.49, 0.91, 0.78))? };
    let row_divider = unsafe { create_brush(target, color(0.25, 0.36, 0.50, 0.075))? };
    unsafe {
        target.FillRoundedRectangle(
            &rounded_rect(rect(bounds.left, bounds.top + 4.0, bounds.left + 3.0, bounds.top + 17.0), 1.5),
            &heading_marker,
        )
    };
    draw_text(
        factory,
        target,
        primary,
        rect(bounds.left + 9.0, bounds.top, bounds.right, bounds.top + 21.0),
        heading,
        12.75,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let available = (bounds.bottom - bounds.top - 23.0).max(1.0);
    let row_height = (available / rows.len().max(1) as f32).min(22.0);
    let label_width = ((bounds.right - bounds.left) * 0.50).clamp(104.0, 142.0);
    for (index, row) in rows.iter().enumerate() {
        let top = bounds.top + 23.0 + index as f32 * row_height;
        if top + row_height > bounds.bottom + 0.5 {
            break;
        }
        if index > 0 {
            unsafe {
                target.DrawLine(
                    Vector2::new(bounds.left, top),
                    Vector2::new(bounds.right, top),
                    &row_divider,
                    0.65,
                    None,
                )
            };
        }
        draw_text(
            factory,
            target,
            muted,
            rect(bounds.left, top, bounds.left + label_width, top + row_height),
            &row.label,
            11.5,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            primary,
            rect(bounds.left + label_width, top, bounds.right, top + row_height),
            &row.value,
            12.25,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_TRAILING,
        )?;
    }
    Ok(())
}

/// 绘制官方账户真实日桶趋势。数据不足两点时由调用方隐藏，避免单点被误读为趋势。
#[allow(clippy::too_many_arguments)]
fn draw_trend_chart(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    points: &[crate::host::NativeTrendPoint],
    title: &str,
    hovered_index: Option<usize>,
    primary: &ID2D1SolidColorBrush,
    muted: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    if points.len() < 2 {
        return Ok(());
    }
    let background = unsafe { create_brush(target, color(0.045, 0.075, 0.14, 0.92))? };
    let border = unsafe { create_brush(target, color(0.24, 0.60, 0.86, 0.30))? };
    unsafe {
        target.FillRoundedRectangle(&rounded_rect(bounds, 8.0), &background);
        target.DrawRoundedRectangle(&rounded_rect(bounds, 8.0), &border, 0.8, None);
    }
    draw_text(
        factory,
        target,
        primary,
        rect(bounds.left + 11.0, bounds.top + 1.0, bounds.right - 112.0, bounds.top + 19.0),
        title,
        11.75,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let plot = trend_plot_bounds(bounds);
    let max_value = points.iter().map(|point| point.value).max().unwrap_or(1).max(1);
    let grid = unsafe { create_brush(target, color(0.25, 0.36, 0.50, 0.105))? };
    let line = unsafe { create_brush(target, color(0.20, 0.49, 0.91, 0.96))? };
    let area = unsafe { create_brush(target, color(0.24, 0.56, 0.94, 0.115))? };
    unsafe {
        for fraction in [0.0_f32, 0.5, 1.0] {
            let y = plot.top + (plot.bottom - plot.top) * fraction;
            target.DrawLine(Vector2::new(plot.left, y), Vector2::new(plot.right, y), &grid, 0.8, None);
        }
    }
    let step = (plot.right - plot.left) / (points.len().saturating_sub(1) as f32);
    let anchors = points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let x = plot.left + step * index as f32;
            let y = plot.bottom - (plot.bottom - plot.top) * point.value as f32 / max_value as f32;
            Vector2::new(x, y)
        })
        .collect::<Vec<_>>();
    // 保形三次插值比逐段 smoothstep 更接近自然趋势曲线：它在真实点之间保持
    // 一阶连续，同时不会为了“圆润”制造高于/低于相邻端点的虚假用量。
    let curve = monotone_cubic_curve(&anchors, 18);
    draw_trend_area(target, &curve, plot.bottom, &area)?;
    for pair in curve.windows(2) {
        unsafe { target.DrawLine(pair[0], pair[1], &line, 2.35, None) };
    }
    for (index, (point, current)) in points.iter().zip(anchors.iter()).enumerate() {
        let highlighted = hovered_index == Some(index);
        if highlighted {
            let guide = unsafe { create_brush(target, color(0.20, 0.49, 0.91, 0.32))? };
            unsafe {
                target.DrawLine(
                    Vector2::new(current.X, plot.top),
                    Vector2::new(current.X, plot.bottom),
                    &guide,
                    1.0,
                    None,
                )
            };
        }
        let dot_radius = if highlighted { 4.0 } else { 2.8 };
        let dot = D2D1_ELLIPSE { point: *current, radiusX: dot_radius, radiusY: dot_radius };
        unsafe {
            if highlighted {
                let halo = create_brush(target, color(0.20, 0.49, 0.91, 0.17))?;
                target.FillEllipse(&D2D1_ELLIPSE { point: *current, radiusX: 8.0, radiusY: 8.0 }, &halo);
            }
            target.FillEllipse(&dot, &line);
        }
        draw_text(
            factory,
            target,
            if highlighted { primary } else { muted },
            rect(current.X - 28.0, plot.bottom + 1.0, current.X + 28.0, bounds.bottom),
            &point.label,
            if highlighted { 9.75 } else { 9.25 },
            if highlighted { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_MEDIUM },
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
    }
    if let Some((index, point)) = hovered_index.and_then(|index| points.get(index).map(|point| (index, point))) {
        let anchor = anchors[index];
        let tooltip_copy = format!("{}  ·  {} Token", point.label, format_grouped_number(point.value));
        let tooltip_format = create_text_format(
            factory,
            10.75,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        )?;
        // Tooltip 承诺展示精确 Token，不能用固定宽度裁掉较大的真实数值。
        // 先用 DirectWrite 实测，再限制在趋势卡片内部；u64 最大值在当前卡宽下
        // 仍可完整容纳。
        let available_width = (bounds.right - bounds.left - 16.0).max(126.0);
        let measured_width = measure_text_width(factory, &tooltip_format, &tooltip_copy, available_width, 34.0)?;
        let tooltip_width = (measured_width + 24.0).clamp(132.0, available_width);
        let tooltip_height = 35.0;
        let tooltip_left =
            (anchor.X - tooltip_width / 2.0).clamp(bounds.left + 8.0, bounds.right - tooltip_width - 8.0);
        let tooltip_top = if anchor.Y - tooltip_height - 7.0 >= bounds.top + 17.0 {
            anchor.Y - tooltip_height - 7.0
        } else {
            (anchor.Y + 8.0).min(bounds.bottom - tooltip_height - 12.0)
        };
        let tooltip = rect(tooltip_left, tooltip_top, tooltip_left + tooltip_width, tooltip_top + tooltip_height);
        let tooltip_bg = unsafe { create_brush(target, color(0.055, 0.085, 0.14, 0.965))? };
        let tooltip_border = unsafe { create_brush(target, color(0.43, 0.70, 0.96, 0.52))? };
        let tooltip_text = unsafe { create_brush(target, color(0.96, 0.98, 1.0, 1.0))? };
        unsafe {
            target.FillRoundedRectangle(&rounded_rect(tooltip, 8.0), &tooltip_bg);
            target.DrawRoundedRectangle(&rounded_rect(tooltip, 8.0), &tooltip_border, 0.9, None);
        }
        draw_text_line(target, &tooltip_format, &tooltip_text, &tooltip, &tooltip_copy);
    }
    draw_text(
        factory,
        target,
        muted,
        rect(bounds.right - 118.0, bounds.top + 1.0, bounds.right - 11.0, bounds.top + 19.0),
        &format!("峰值 {}", compact_token_value(max_value)),
        9.75,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    )?;
    Ok(())
}

/// 使用保形三次 Hermite 插值生成平滑趋势线。
///
/// 切线采用 PCHIP 的符号与幅度约束：局部极值处切线归零，同向区间使用调和
/// 平均。因此曲线穿过全部真实点，并且每个区间都限制在相邻端点值范围内。
fn monotone_cubic_curve(anchors: &[Vector2], subdivisions: usize) -> Vec<Vector2> {
    if anchors.len() < 2 || subdivisions == 0 {
        return anchors.to_vec();
    }
    let tangents = monotone_tangents(anchors);
    let mut curve = Vec::with_capacity((anchors.len() - 1) * subdivisions + 1);
    curve.push(anchors[0]);
    for (index, pair) in anchors.windows(2).enumerate() {
        let width = (pair[1].X - pair[0].X).max(f32::EPSILON);
        for step in 1..=subdivisions {
            let t = step as f32 / subdivisions as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            let y =
                h00 * pair[0].Y + h10 * width * tangents[index] + h01 * pair[1].Y + h11 * width * tangents[index + 1];
            curve.push(Vector2::new(pair[0].X + width * t, y));
        }
    }
    curve
}

fn monotone_tangents(anchors: &[Vector2]) -> Vec<f32> {
    if anchors.len() == 2 {
        let slope = segment_slope(anchors[0], anchors[1]);
        return vec![slope, slope];
    }
    let slopes = anchors.windows(2).map(|pair| segment_slope(pair[0], pair[1])).collect::<Vec<_>>();
    let mut tangents = vec![0.0; anchors.len()];
    tangents[0] = endpoint_tangent(anchors[1].X - anchors[0].X, anchors[2].X - anchors[1].X, slopes[0], slopes[1]);
    let last = anchors.len() - 1;
    tangents[last] = endpoint_tangent(
        anchors[last].X - anchors[last - 1].X,
        anchors[last - 1].X - anchors[last - 2].X,
        slopes[last - 1],
        slopes[last - 2],
    );
    for index in 1..last {
        let before = slopes[index - 1];
        let after = slopes[index];
        if before == 0.0 || after == 0.0 || before.signum() != after.signum() {
            tangents[index] = 0.0;
            continue;
        }
        let before_width = anchors[index].X - anchors[index - 1].X;
        let after_width = anchors[index + 1].X - anchors[index].X;
        let weight_before = 2.0 * after_width + before_width;
        let weight_after = after_width + 2.0 * before_width;
        tangents[index] = (weight_before + weight_after) / (weight_before / before + weight_after / after);
    }
    tangents
}

fn segment_slope(left: Vector2, right: Vector2) -> f32 {
    (right.Y - left.Y) / (right.X - left.X).max(f32::EPSILON)
}

fn endpoint_tangent(endpoint_width: f32, adjacent_width: f32, endpoint_slope: f32, adjacent_slope: f32) -> f32 {
    let mut tangent = ((2.0 * endpoint_width + adjacent_width) * endpoint_slope - endpoint_width * adjacent_slope)
        / (endpoint_width + adjacent_width).max(f32::EPSILON);
    if tangent.signum() != endpoint_slope.signum() {
        tangent = 0.0;
    } else if endpoint_slope.signum() != adjacent_slope.signum() && tangent.abs() > 3.0 * endpoint_slope.abs() {
        tangent = 3.0 * endpoint_slope;
    }
    tangent
}

/// 为趋势线补一层低透明面积阴影，帮助用户快速辨认峰谷，但不与精确数据点争夺
/// 视觉注意力。几何严格使用同一批曲线采样点，避免阴影和折线产生错位。
fn draw_trend_area(
    target: &ID2D1RenderTarget,
    curve: &[Vector2],
    baseline: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let Some((first, rest)) = curve.split_first() else {
        return Ok(());
    };
    let last = *curve.last().unwrap_or(first);
    unsafe {
        let factory = target.GetFactory()?;
        let geometry = factory.CreatePathGeometry()?;
        let sink = geometry.Open()?;
        sink.BeginFigure(Vector2::new(first.X, baseline), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(*first);
        sink.AddLines(rest);
        sink.AddLine(Vector2::new(last.X, baseline));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, brush, None::<&ID2D1Brush>);
    }
    Ok(())
}

fn trend_plot_bounds(bounds: D2D_RECT_F) -> D2D_RECT_F {
    rect(bounds.left + 14.0, bounds.top + 22.0, bounds.right - 14.0, bounds.bottom - 15.0)
}

fn format_grouped_number(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }
    output
}

/// 把详情窗口客户区物理像素转换回 880×590 DIP 设计坐标，并返回鼠标最近的
/// 真实日桶。只有位于趋势图绘图区时命中，避免卡片其他区域意外弹出提示。
#[must_use]
pub(crate) fn details_trend_hit_test(
    client_size_px: (u32, u32),
    dpi: f32,
    details: &NativeHostDetails,
    mouse_px: (i32, i32),
) -> Option<usize> {
    if !details.compact_primary_column || details.trend_points.len() < 2 || dpi <= 0.0 {
        return None;
    }
    let point = details_layout_point(client_size_px, dpi, mouse_px)?;
    let bounds = compact_official_trend_bounds(details)?;
    let plot = trend_plot_bounds(bounds);
    if point.X < plot.left - 6.0
        || point.X > plot.right + 6.0
        || point.Y < plot.top - 8.0
        || point.Y > plot.bottom + 10.0
    {
        return None;
    }
    let step = (plot.right - plot.left) / (details.trend_points.len() - 1) as f32;
    Some(((point.X - plot.left) / step).round().clamp(0.0, (details.trend_points.len() - 1) as f32) as usize)
}

/// 把详情卡客户区鼠标位置映射为顶部操作入口。命中几何和绘制几何共用
/// `details_action_bounds`，并复用与详情渲染相同的 DPI/等比缩放变换。
#[must_use]
pub(crate) fn details_action_hit_test(
    client_size_px: (u32, u32),
    dpi: f32,
    mouse_px: (i32, i32),
) -> Option<DetailsAction> {
    let point = details_layout_point(client_size_px, dpi, mouse_px)?;
    [DetailsAction::Refresh, DetailsAction::OpenSettings]
        .into_iter()
        .find(|action| point_in_rect(point, details_action_bounds(*action)))
}

fn details_layout_point(client_size_px: (u32, u32), dpi: f32, mouse_px: (i32, i32)) -> Option<Vector2> {
    if client_size_px.0 == 0 || client_size_px.1 == 0 || dpi <= 0.0 {
        return None;
    }
    let dip_scale = 96.0 / dpi;
    let surface = (client_size_px.0 as f32 * dip_scale, client_size_px.1 as f32 * dip_scale);
    let transform = fit_layout_transform(surface, DETAILS_LAYOUT_SIZE);
    let scale = transform.M11;
    if scale <= 0.0 {
        return None;
    }
    Some(Vector2::new(
        (mouse_px.0 as f32 * dip_scale - transform.M31) / scale,
        (mouse_px.1 as f32 * dip_scale - transform.M32) / scale,
    ))
}

fn point_in_rect(point: Vector2, bounds: D2D_RECT_F) -> bool {
    point.X >= bounds.left && point.X <= bounds.right && point.Y >= bounds.top && point.Y <= bounds.bottom
}

fn compact_official_trend_bounds(details: &NativeHostDetails) -> Option<D2D_RECT_F> {
    if details.trend_points.len() < 2 {
        return None;
    }
    let card = rect(10.0, 8.0, DETAILS_LAYOUT_SIZE.0 - 10.0, DETAILS_LAYOUT_SIZE.1 - 12.0);
    let left = card.left + 16.0;
    let right = card.right - 16.0;
    let body_top = card.top + 68.0;
    let health_top = card.bottom - 57.0;
    let body_bottom = health_top - 8.0;
    let left_width = compact_official_left_width(right - left);
    let secondary_left = left + left_width + 18.0;
    let hero_bottom = body_top + 28.0 + 82.0;
    let usage_panel_top = hero_bottom + 8.0;
    let content_left = secondary_left + 12.0;
    let content_right = right - 12.0;
    let content_top = usage_panel_top + 8.0;
    let content_bottom = body_bottom - 8.0;
    let chart_top = content_bottom - 50.0;
    let detail_bottom = content_top + 132.0;
    Some(rect(content_left, detail_bottom + 8.0, content_right, chart_top - 8.0))
}

fn draw_token_strip_content(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    size: (f32, f32),
    details: &NativeHostDetails,
) -> Result<(), Error> {
    // 最后 13 DIP 专门留给朝下的指向尖角。卡片本身不占满画布，保证 layered
    // window 周围仍是真透明像素，不会留下旧版本那种透明矩形轮廓。
    let bounds = rect(5.0, 3.0, size.0 - 5.0, size.1 - 14.0);
    let shadow = unsafe { create_brush(target, color(0.0, 0.0, 0.0, 0.28))? };
    let background = unsafe { create_brush(target, color(0.035, 0.047, 0.090, 0.985))? };
    let border = unsafe { create_brush(target, color(0.34, 0.64, 0.96, 0.36))? };
    let label = unsafe { create_brush(target, color(0.57, 0.68, 0.84, 0.94))? };
    let value = unsafe { create_brush(target, color(0.95, 0.98, 1.0, 1.0))? };
    let accent = unsafe { create_brush(target, color(0.35, 0.92, 0.93, 0.96))? };
    unsafe {
        target.DrawRoundedRectangle(
            &rounded_rect(rect(bounds.left, bounds.top + 2.0, bounds.right, bounds.bottom + 2.0), 13.0),
            &shadow,
            5.0,
            None,
        );
        target.FillRoundedRectangle(&rounded_rect(bounds, 13.0), &background);
        target.DrawRoundedRectangle(&rounded_rect(bounds, 13.0), &border, 1.0, None);
    }
    draw_token_strip_pointer(target, bounds, &background, &border)?;
    draw_text(
        factory,
        target,
        &accent,
        rect(bounds.left + 16.0, bounds.top + 7.0, bounds.right - 16.0, bounds.top + 27.0),
        "本次 Token 消耗",
        12.0,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let rows = if details.quick_rows.is_empty() { &details.secondary_rows } else { &details.quick_rows };
    let shown = rows.iter().take(6).collect::<Vec<_>>();
    let cell_width = (bounds.right - bounds.left - 24.0) / shown.len().max(1) as f32;
    for (index, row) in shown.iter().enumerate() {
        let left = bounds.left + 12.0 + index as f32 * cell_width;
        let right = left + cell_width - 2.0;
        if index > 0 {
            let separator = unsafe { create_brush(target, color(0.37, 0.53, 0.76, 0.20))? };
            unsafe {
                target.DrawLine(
                    Vector2::new(left - 7.0, bounds.top + 34.0),
                    Vector2::new(left - 7.0, bounds.bottom - 12.0),
                    &separator,
                    0.75,
                    None,
                );
            }
        }
        draw_text(
            factory,
            target,
            &label,
            rect(left, bounds.top + 35.0, right, bounds.top + 53.0),
            &row.label,
            10.5,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
        draw_text(
            factory,
            target,
            &value,
            rect(left, bounds.top + 53.0, right, bounds.bottom - 10.0),
            &row.value,
            15.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
    }
    Ok(())
}

/// 绘制自动浮窗底边的朝下指针。它不接受鼠标事件，只是在视觉上把“本次”
/// 消耗与下方任务栏组件关联，不能做成向上的 tooltip 箭头。
fn draw_token_strip_pointer(
    target: &ID2D1RenderTarget,
    card: D2D_RECT_F,
    fill: &ID2D1SolidColorBrush,
    border: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let center = (card.left + card.right) / 2.0;
    let left = Vector2::new(center - 8.0, card.bottom - 0.5);
    let tip = Vector2::new(center, card.bottom + 10.0);
    let right = Vector2::new(center + 8.0, card.bottom - 0.5);
    let factory = unsafe { target.GetFactory()? };
    let geometry = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(left, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(tip);
        sink.AddLine(right);
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, fill, None::<&ID2D1Brush>);
        target.DrawLine(left, tip, border, 0.9, None);
        target.DrawLine(tip, right, border, 0.9, None);
    }
    Ok(())
}

/// 绘制 Token 构成图：普通输入、缓存输入、输出使用互斥权重，
/// 因此缓存输入不会再次叠加到普通输入中。图表保持轻量，不引入第三方图表库。
fn draw_token_chart(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    segments: &[crate::host::NativeChartSegment],
    title: &str,
    primary: &ID2D1SolidColorBrush,
    muted: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    use crate::host::NativeChartTone;

    let total = segments.iter().map(|segment| segment.value).sum::<u64>();
    if total == 0 {
        return Ok(());
    }
    if !title.is_empty() {
        draw_text(
            factory,
            target,
            primary,
            rect(bounds.left, bounds.top, bounds.right, bounds.top + 15.0),
            title,
            11.25,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
    }
    let bar_top = bounds.top + 17.0;
    let bar_bottom = bar_top + 10.0;
    let bar_left = bounds.left;
    let bar_right = bounds.right;
    let track = unsafe { create_brush(target, color(0.12, 0.18, 0.24, 0.10))? };
    unsafe { target.FillRoundedRectangle(&rounded_rect(rect(bar_left, bar_top, bar_right, bar_bottom), 5.0), &track) };

    let mut x = bar_left;
    for segment in segments {
        let width = (bar_right - bar_left) * segment.value as f32 / total as f32;
        if width <= 0.0 {
            continue;
        }
        let segment_color = match segment.tone {
            NativeChartTone::Input => color(0.25, 0.55, 0.90, 0.92),
            NativeChartTone::Cached => color(0.43, 0.70, 0.93, 0.92),
            NativeChartTone::Output => color(0.25, 0.68, 0.43, 0.94),
        };
        let brush = unsafe { create_brush(target, segment_color)? };
        let end = (x + width).min(bar_right);
        unsafe { target.FillRoundedRectangle(&rounded_rect(rect(x, bar_top, end, bar_bottom), 5.0), &brush) };
        x = end;
    }

    let mut legend_x = bar_left;
    let legend_top = bar_bottom + 4.0;
    for segment in segments {
        let tone = match segment.tone {
            NativeChartTone::Input => color(0.25, 0.55, 0.90, 0.92),
            NativeChartTone::Cached => color(0.43, 0.70, 0.93, 0.92),
            NativeChartTone::Output => color(0.25, 0.68, 0.43, 0.94),
        };
        let swatch = unsafe { create_brush(target, tone)? };
        let dot = D2D1_ELLIPSE { point: Vector2::new(legend_x + 3.5, legend_top + 6.0), radiusX: 3.5, radiusY: 3.5 };
        unsafe { target.FillEllipse(&dot, &swatch) };
        let label_width = 82.0_f32.min((bounds.right - legend_x - 8.0).max(20.0));
        draw_text(
            factory,
            target,
            muted,
            rect(legend_x + 10.0, legend_top, legend_x + 10.0 + label_width, legend_top + 14.0),
            &format!("{} {}", segment.label, compact_token_value(segment.value)),
            10.25,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        legend_x += label_width + 24.0;
        if legend_x >= bounds.right - 18.0 {
            break;
        }
    }
    // 在图表右侧保留总量，帮助用户把横条与当前图表口径关联起来。
    draw_text(
        factory,
        target,
        primary,
        rect((bounds.right - 112.0).max(legend_x), legend_top, bounds.right, legend_top + 14.0),
        &format!("合计 {}", compact_token_value(total)),
        10.25,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    )?;
    Ok(())
}

fn compact_token_value(value: u64) -> String {
    // 与任务栏摘要使用同一截断规则，避免同一个 31_471 在一处显示 31.4K、
    // 另一处因浮点四舍五入显示 31.5K，看起来像数据源不一致。
    format_compact_number(value)
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_column(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    heading: &str,
    rows: &[crate::host::NativeDetailRow],
    muted: &ID2D1SolidColorBrush,
    primary: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    draw_text(
        factory,
        target,
        primary,
        rect(bounds.left, bounds.top, bounds.right, bounds.top + 22.0),
        heading,
        13.25,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    let available = (bounds.bottom - bounds.top - 25.0).max(1.0);
    let row_height = (available / rows.len().max(1) as f32).min(32.0);
    // 详情卡会出现“最近一次 / 上下文”这类长中文标签；适度扩大标签列，
    // 优先保证可读性，剩余宽度仍留给数值列，不用省略号制造歧义。
    let label_width = ((bounds.right - bounds.left) * 0.46).clamp(96.0, 150.0);
    for (index, row) in rows.iter().enumerate() {
        let top = bounds.top + 25.0 + index as f32 * row_height;
        // 行高会按内容数量自适应；11 行布局通常略低于 20 DIP。
        // 这里按实际行高判断并保留半像素容差，避免最后一行因浮点误差被误裁掉。
        if top + row_height > bounds.bottom + 0.5 {
            break;
        }
        if row.kind == NativeDetailRowKind::Section {
            unsafe {
                target.DrawLine(
                    Vector2::new(bounds.left, top + 2.0),
                    Vector2::new(bounds.right, top + 2.0),
                    muted,
                    0.75,
                    None,
                )
            };
            draw_text(
                factory,
                target,
                primary,
                rect(bounds.left, top + 3.0, bounds.right, top + row_height),
                &row.label,
                12.25,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            continue;
        }
        draw_text(
            factory,
            target,
            muted,
            rect(bounds.left, top, bounds.left + label_width, top + row_height),
            &row.label,
            12.75,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
        draw_text(
            factory,
            target,
            primary,
            rect(bounds.left + label_width, top, bounds.right, top + row_height),
            &row.value,
            13.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    bounds: D2D_RECT_F,
    value: &str,
    size: f32,
    weight: windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT,
    alignment: windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT,
) -> Result<(), Error> {
    let format = create_text_format(factory, size, weight, alignment, DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    draw_text_line(target, &format, brush, &bounds, value);
    Ok(())
}

const fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

const fn rounded_rect(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius }
}

impl LayeredSurface {
    fn new(width_px: u32, height_px: u32) -> Result<Self, Error> {
        let width = i32::try_from(width_px).map_err(|_| Error::from_thread())?;
        let height = i32::try_from(height_px).map_err(|_| Error::from_thread())?;
        let memory_dc = unsafe { CreateCompatibleDC(None) };
        if memory_dc.0.is_null() {
            return Err(Error::from_thread());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = match unsafe { CreateDIBSection(Some(memory_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                unsafe {
                    let _ = DeleteDC(memory_dc);
                }
                return Err(error);
            }
        };
        let old_bitmap = unsafe { SelectObject(memory_dc, HGDIOBJ(bitmap.0)) };
        Ok(Self { dc_target: None, render_target: None, memory_dc, bitmap, old_bitmap, width_px, height_px })
    }

    fn present(&self, hwnd: HWND) -> Result<(), Error> {
        let size = SIZE { cx: self.width_px as i32, cy: self.height_px as i32 };
        let source = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(self.memory_dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        self.render_target = None;
        self.dc_target = None;
        unsafe {
            if !self.old_bitmap.0.is_null() {
                let _ = SelectObject(self.memory_dc, self.old_bitmap);
            }
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory_dc);
        }
    }
}

unsafe fn create_brush(target: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush, Error> {
    unsafe { target.CreateSolidColorBrush(&color, None) }
}

#[allow(dead_code)]
unsafe fn draw_ring(
    target: &ID2D1RenderTarget,
    ring: RingArc,
    track: &ID2D1SolidColorBrush,
    progress: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    unsafe {
        draw_arc(target, ring, core::f32::consts::TAU, track)?;
        if let Some(sweep) = ring.sweep_angle {
            draw_arc(target, ring, sweep, progress)?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
unsafe fn draw_arc(
    target: &ID2D1RenderTarget,
    ring: RingArc,
    sweep: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let sweep = sweep.clamp(0.0, core::f32::consts::TAU);
    if sweep <= 0.0 || ring.radius <= 0.0 || ring.stroke_width <= 0.0 {
        return Ok(());
    }
    // 完整圆不能再叠加首尾 round cap，否则 100% 会在 12 点方向形成凸块。
    if (core::f32::consts::TAU - sweep).abs() < 0.001 {
        let circle = D2D1_ELLIPSE {
            point: Vector2::new(ring.center.x, ring.center.y),
            radiusX: ring.radius,
            radiusY: ring.radius,
        };
        unsafe { target.DrawEllipse(&circle, brush, ring.stroke_width, None) };
        return Ok(());
    }
    let segments = ((sweep / core::f32::consts::TAU) * 72.0).ceil().max(1.0) as u32;
    let point = |angle: f32| -> Vector2 {
        Vector2::new(ring.center.x + ring.radius * angle.cos(), ring.center.y + ring.radius * angle.sin())
    };
    let start = point(ring.start_angle);
    let mut previous = start;
    for step in 1..=segments {
        let angle = ring.start_angle + sweep * step as f32 / segments as f32;
        let next = point(angle);
        unsafe { target.DrawLine(previous, next, brush, ring.stroke_width, None) };
        previous = next;
    }
    unsafe { draw_round_cap(target, start, ring.stroke_width * 0.5, brush) };
    unsafe { draw_round_cap(target, previous, ring.stroke_width * 0.5, brush) };
    Ok(())
}

#[allow(dead_code)]
unsafe fn draw_round_cap(target: &ID2D1RenderTarget, point: Vector2, radius: f32, brush: &ID2D1SolidColorBrush) {
    let cap = D2D1_ELLIPSE { point, radiusX: radius.max(1.0), radiusY: radius.max(1.0) };
    unsafe { target.FillEllipse(&cap, brush) };
}

#[allow(dead_code)]
unsafe fn draw_lamp(target: &ID2D1RenderTarget, lamp: ActivityLampModel) -> Result<(), Error> {
    let lamp_color = lamp_color(lamp.color);
    let core_brush = unsafe { create_brush(target, lamp_color)? };
    if let Some(glow) = lamp.glow {
        let glow_brush = unsafe { create_brush(target, lamp_color)? };
        unsafe { glow_brush.SetOpacity(glow.opacity) };
        for factor in [1.0_f32, 0.72, 0.46] {
            let ellipse = ellipse(Circle { center: glow.circle.center, radius: glow.circle.radius * factor });
            unsafe { target.DrawEllipse(&ellipse, &glow_brush, 1.0 + factor, None) };
        }
    }
    let core = ellipse(lamp.core);
    unsafe { target.FillEllipse(&core, &core_brush) };
    Ok(())
}

/// 绘制任务栏中的单一流式额度胶囊。
///
/// GIF 基准要求的并不是横向海浪，而是按“已消耗宽度”推进的竖直能量边界：
/// 紫色 5 小时层位于底部，状态色 Weekly 层位于前景，两者同处一枚椭圆内。
/// 这里不引入 WebGL；Direct2D 仅使用一条连续路径、三层柔光描边和少量粒子，
/// 因而不会增加常驻浏览器/着色器运行时或高频磁盘写入。
fn draw_fluid_quota(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    fluid: FluidQuotaModel,
    background_opacity: f32,
    details: &NativeHostDetails,
) -> Result<(), Error> {
    let bounds = rect(fluid.bounds.left, fluid.bounds.top, fluid.bounds.right, fluid.bounds.bottom);
    // 未消耗区使用可配置的深色玻璃，而不是纯黑。色值保持深海军蓝，alpha
    // 默认 70%，让桌面背景有轻微透出但不降低文字可读性。
    let background = unsafe { create_brush(target, color(0.035, 0.048, 0.078, background_opacity.clamp(0.20, 1.0)))? };
    let border = unsafe { create_brush(target, color(0.52, 0.66, 0.88, 0.48))? };
    let text_primary = unsafe { create_brush(target, color(0.95, 0.98, 1.0, 0.98))? };
    let text_secondary = unsafe { create_brush(target, color(0.80, 0.88, 0.98, 0.90))? };
    // 任务栏主体必须是完整椭圆胶囊，而不是早期圆角矩形。波面绘制会按同一
    // 胶囊边界裁切，避免紫色/青色填充在两端露出方形角。
    let radius = ((bounds.bottom - bounds.top) / 2.0).max(1.0);
    unsafe {
        target.FillRoundedRectangle(&rounded_rect(bounds, radius), &background);
        target.DrawRoundedRectangle(&rounded_rect(bounds, radius), &border, 0.9, None);
    }

    let inner = rect(bounds.left + 1.0, bounds.top + 1.0, bounds.right - 1.0, bounds.bottom - 1.0);
    if let Some(remaining) = fluid.five_hour_remaining_percent {
        // 5 小时层只作为同一平面的低对比紫色底流。它不单独占一行，也不应在
        // 官方明确没有 5 小时窗口时留下一层假进度。
        draw_flow_layer(
            target,
            inner,
            remaining,
            fluid.phase * 0.81 + 1.70,
            FlowLayerStyle::new(2.0, color(0.40, 0.22, 0.94, 0.34), color(0.72, 0.56, 1.0, 0.84), false),
        )?;
    }
    if let Some(remaining) = fluid.weekly_remaining_percent {
        // 沙滩浪线的变化应细腻且连续：额度边界只允许低振幅地涨落，不能像闪电
        // 一样大幅横摆。双层时再略降低，避免两条边缘互相抢戏。
        let amplitude = if fluid.five_hour_remaining_percent.is_some() { 2.15 } else { 2.75 };
        let current_style = FlowLayerStyle::new(
            amplitude,
            fluid_foreground_color(fluid.activity),
            fluid_highlight_color(fluid.activity),
            true,
        );
        if fluid.previous_activity != fluid.activity && fluid.state_transition_progress < 1.0 {
            let previous_style = FlowLayerStyle::new(
                amplitude,
                fluid_foreground_color(fluid.previous_activity),
                fluid_highlight_color(fluid.previous_activity),
                true,
            );
            draw_flow_layer(target, inner, remaining, fluid.phase, previous_style)?;
            draw_state_transition_wave(
                target,
                inner,
                remaining,
                fluid.phase,
                fluid.state_transition_progress,
                current_style,
            )?;
        } else {
            draw_flow_layer(target, inner, remaining, fluid.phase, current_style)?;
        }
    } else {
        let unavailable = unsafe { create_brush(target, color(0.33, 0.43, 0.56, 0.24))? };
        // 没有可信 Weekly 数字时只保留低对比底色，不能把“未知”冒充为已耗尽。
        unsafe { target.FillRoundedRectangle(&rounded_rect(inner, radius - 1.0), &unavailable) };
    }

    // 文字直接悬在叠浪上。设计稿中的四块信息保持稳定锚点：今日/缓存在左，
    // Credits 在中部，Weekly 与 5h 在右；没有任何半透明读取框或额外轮廓。
    let bounds_width = (bounds.right - bounds.left).max(1.0);
    let (line1, line2) = summary_lines(details);
    let (cache_line, credits_line) = split_taskbar_secondary(&line2);
    draw_text(
        factory,
        target,
        &text_primary,
        rect(bounds.left + 18.0, bounds.top + 7.0, bounds.left + bounds_width * 0.30, bounds.top + 27.0),
        &line1,
        14.0,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    draw_text(
        factory,
        target,
        &text_secondary,
        rect(bounds.left + 18.0, bounds.top + 26.0, bounds.left + bounds_width * 0.30, bounds.bottom - 6.0),
        &cache_line,
        11.5,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    )?;
    if let Some(credits) = credits_line {
        draw_text(
            factory,
            target,
            &text_primary,
            rect(
                bounds.left + bounds_width * 0.32,
                bounds.top + 13.0,
                bounds.left + bounds_width * 0.58,
                bounds.bottom - 7.0,
            ),
            &credits,
            12.0,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
    }

    let weekly_text =
        fluid.weekly_remaining_percent.map_or_else(|| "--".to_owned(), |value| format!("{}%", value.round()));
    draw_text(
        factory,
        target,
        &text_primary,
        rect(bounds.right - 126.0, bounds.top + 5.0, bounds.right - 15.0, bounds.top + 29.0),
        &weekly_text,
        22.0,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    )?;
    let five_hour_text =
        fluid.five_hour_remaining_percent.map_or_else(String::new, |value| format!("5小时 {}%", value.round()));
    draw_text(
        factory,
        target,
        &text_secondary,
        rect(bounds.right - 126.0, bounds.top + 28.0, bounds.right - 15.0, bounds.bottom - 4.0),
        &five_hour_text,
        11.0,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    )?;
    Ok(())
}

/// 单层流场的视觉参数。把颜色、振幅和层级收纳在一起，避免渲染函数同时承担
/// 过多无关联参数，也便于详情卡未来复用同一动画语义。
#[derive(Debug, Clone, Copy)]
struct FlowLayerStyle {
    amplitude: f32,
    fill_color: D2D1_COLOR_F,
    highlight_color: D2D1_COLOR_F,
    foreground: bool,
}

impl FlowLayerStyle {
    const fn new(amplitude: f32, fill_color: D2D1_COLOR_F, highlight_color: D2D1_COLOR_F, foreground: bool) -> Self {
        Self { amplitude, fill_color, highlight_color, foreground }
    }
}

/// 按 GIF 的“竖直流式边界”绘制一层额度。业务模型仍提供剩余百分比，但视觉
/// 宽度必须反向使用已消耗比例；右侧文字继续显示剩余百分比，两个语义不混用。
fn draw_flow_layer(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    style: FlowLayerStyle,
) -> Result<(), Error> {
    let fill = unsafe { create_brush(target, style.fill_color)? };
    let outer_glow = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.12 } else { 0.08 },
            ),
        )?
    };
    let middle_glow = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.34 } else { 0.24 },
            ),
        )?
    };
    let core = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.96 } else { 0.76 },
            ),
        )?
    };
    let roll_soft = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.075 } else { 0.045 },
            ),
        )?
    };
    let roll_middle = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.16 } else { 0.10 },
            ),
        )?
    };
    let roll_core = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.27 } else { 0.16 },
            ),
        )?
    };
    let shore_wash = unsafe {
        create_brush(
            target,
            color(
                style.highlight_color.r,
                style.highlight_color.g,
                style.highlight_color.b,
                if style.foreground { 0.18 } else { 0.095 },
            ),
        )?
    };
    // 泡沫只沿真实的拍岸外缘绘制。比填充层更亮，却不使用离散粒子，避免在
    // 60 FPS 下形成闪电、噪点或突兀的白色斑块。
    let shore_foam = unsafe {
        create_brush(
            target,
            color(
                (0.64 + style.highlight_color.r * 0.36).min(1.0),
                (0.70 + style.highlight_color.g * 0.30).min(1.0),
                (0.78 + style.highlight_color.b * 0.22).min(1.0),
                if style.foreground { 0.74 } else { 0.42 },
            ),
        )?
    };

    draw_flow_fill(target, bounds, remaining_percent, phase, style.amplitude, &fill)?;
    // 先绘制水体内三层高低错落的横向涌浪。它们形成海水的纵深、起伏和流动感，
    // 不承担进度边界，避免把内部动画简化成一根移动曲线。
    draw_ocean_swell_layers(target, bounds, remaining_percent, phase, style.amplitude, &roll_soft, &roll_middle)?;
    // 再以少量竖向推进浪连接到前沿：这是抵岸主浪的来向，不再用密集条纹填满。
    draw_flow_rolls(target, bounds, remaining_percent, phase, style.amplitude, &roll_soft, 4.3)?;
    draw_flow_rolls(target, bounds, remaining_percent, phase, style.amplitude, &roll_middle, 1.8)?;
    draw_flow_rolls(target, bounds, remaining_percent, phase, style.amplitude, &roll_core, 0.55)?;
    // 翻滚浪抵达真实额度边沿时，单独绘制一层淡色“拍岸水花”。它可略微越过
    // 前沿、随后随影响力衰退而收回；不能与内部翻滚波带共用同一曲线。
    draw_shoreline_wash(target, bounds, remaining_percent, phase, style.amplitude, &shore_wash, &shore_foam)?;
    // 由外至内三条连续路径形成柔和发光边界。它们不是多根竖条，因此在任务栏
    // 的小尺寸及高 DPI 下不会出现原先那种栅格感。
    draw_flow_boundary(target, bounds, remaining_percent, phase, style.amplitude, &outer_glow, 5.4)?;
    draw_flow_boundary(target, bounds, remaining_percent, phase, style.amplitude, &middle_glow, 2.6)?;
    draw_flow_boundary(target, bounds, remaining_percent, phase, style.amplitude, &core, 0.92)?;
    Ok(())
}

/// 状态颜色的流体替换层。额度的真实前沿保持不动；仅在已消耗色块内部，让携带
/// 新状态色的一股浪从左向右翻滚，经过的区域才被替换。这样“执行 → 空闲”等
/// 变化看起来是新海浪推进，而不是突兀换色。
fn draw_state_transition_wave(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    progress: f32,
    style: FlowLayerStyle,
) -> Result<(), Error> {
    let fill = unsafe { create_brush(target, style.fill_color)? };
    let outer = unsafe {
        create_brush(target, color(style.highlight_color.r, style.highlight_color.g, style.highlight_color.b, 0.13))?
    };
    let middle = unsafe {
        create_brush(target, color(style.highlight_color.r, style.highlight_color.g, style.highlight_color.b, 0.40))?
    };
    let core = unsafe {
        create_brush(target, color(style.highlight_color.r, style.highlight_color.g, style.highlight_color.b, 0.92))?
    };
    draw_transition_fill(target, bounds, remaining_percent, phase, progress, style.amplitude * 1.42, &fill)?;
    draw_transition_boundary(target, bounds, remaining_percent, phase, progress, style.amplitude * 1.42, &outer, 6.0)?;
    draw_transition_boundary(target, bounds, remaining_percent, phase, progress, style.amplitude * 1.42, &middle, 2.8)?;
    draw_transition_boundary(target, bounds, remaining_percent, phase, progress, style.amplitude * 1.42, &core, 0.95)?;
    Ok(())
}

fn draw_transition_fill(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    progress: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let left_point = |step: usize| {
        let y = bounds.top + (bounds.bottom - bounds.top) * step as f32 / SEGMENTS as f32;
        Vector2::new(capsule_horizontal_bounds(bounds, y).0, y)
    };
    let front = |step: usize| {
        transition_boundary_point(bounds, remaining_percent, phase, progress, amplitude, step as f32 / SEGMENTS as f32)
    };
    let factory = unsafe { target.GetFactory()? };
    let geometry = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(left_point(0), D2D1_FIGURE_BEGIN_FILLED);
        for step in 1..=SEGMENTS {
            sink.AddLine(left_point(step));
        }
        for step in (0..=SEGMENTS).rev() {
            sink.AddLine(front(step));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, brush, None::<&ID2D1Brush>);
    }
    Ok(())
}

// 参数全部属于一次绘制快照；为了不把瞬态 D2D brush/几何状态提升为共享结构，
// 保持显式参数边界并在此处局部豁免 Clippy 的 7 参数建议。
#[allow(clippy::too_many_arguments)]
fn draw_transition_boundary(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    progress: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
    stroke_width: f32,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let mut previous = transition_boundary_point(bounds, remaining_percent, phase, progress, amplitude, 0.0);
    for step in 1..=SEGMENTS {
        let next = transition_boundary_point(
            bounds,
            remaining_percent,
            phase,
            progress,
            amplitude,
            step as f32 / SEGMENTS as f32,
        );
        unsafe { target.DrawLine(previous, next, brush, stroke_width, None) };
        previous = next;
    }
    Ok(())
}

/// 颜色替换浪的前沿：基础位置按替换进度左→右推进，但永远不会越过真实额度
/// 前沿；叠加的卷曲只影响视觉边界，不能暗中改变百分比的业务含义。
fn transition_boundary_point(
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    progress: f32,
    amplitude: f32,
    y_fraction: f32,
) -> Vector2 {
    let y_fraction = y_fraction.clamp(0.0, 1.0);
    let y = bounds.top + (bounds.bottom - bounds.top) * y_fraction;
    let (left, right) = capsule_horizontal_bounds(bounds, y);
    let quota_front = flow_boundary_point(bounds, remaining_percent, phase, amplitude * 0.72, y_fraction);
    let span = (quota_front.X - left).max(0.0);
    let wave = core::f32::consts::TAU;
    let curl = (y_fraction * wave * 1.72 - phase * 0.46).sin() * amplitude
        + (y_fraction * wave * 3.93 + phase * 0.21 + 0.83).sin() * amplitude * 0.31
        + (y_fraction * wave * 7.18 - phase * 0.09 + 1.71).sin() * amplitude * 0.11;
    let x = (left + span * progress.clamp(0.0, 1.0) + curl).clamp(left, quota_front.X.min(right));
    Vector2::new(x, y)
}

/// 在已消耗水体中叠加三层横向涌浪。每层都有不同的高度、周期、速度和相位，
/// 因此像水面上错落的波峰/波谷，而不是把进度条内部简化成一条移动曲线。
fn draw_ocean_swell_layers(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    fill_brush: &ID2D1SolidColorBrush,
    crest_brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    const SEGMENTS: usize = 72;
    const LANE_CENTERS: [f32; 3] = [0.27, 0.53, 0.76];
    const LANE_SEEDS: [f32; 3] = [0.31, 2.47, 4.83];
    let height = (bounds.bottom - bounds.top).max(1.0);
    let left = bounds.left + height * 0.23;
    // 给真实前沿留出一点空间，避免横向水纹越过额度边界而像错误进度。
    let right = (flow_boundary_base_x(bounds, remaining_percent) - amplitude * 1.3 - 1.0).max(left);
    if right - left < 18.0 {
        return Ok(());
    }

    for lane in 0..LANE_CENTERS.len() {
        let surface = |fraction: f32| {
            let x = left + (right - left) * fraction;
            let seed = LANE_SEEDS[lane];
            let swell = (fraction * core::f32::consts::TAU * (1.04 + lane as f32 * 0.17)
                - phase * (0.47 + lane as f32 * 0.06)
                + seed)
                .sin()
                * (1.18 + amplitude * 0.13)
                + (fraction * core::f32::consts::TAU * (2.81 + lane as f32 * 0.31)
                    + phase * (0.21 + lane as f32 * 0.04)
                    + seed * 1.7)
                    .sin()
                    * 0.52;
            let y = bounds.top + height * LANE_CENTERS[lane] + swell;
            (x, y)
        };
        let thickness = |fraction: f32| {
            (1.10 + 0.32 * (fraction * core::f32::consts::TAU * 1.67 + phase * 0.29 + LANE_SEEDS[lane]).sin())
                .clamp(0.62, 1.42)
        };
        let factory = unsafe { target.GetFactory()? };
        let geometry = unsafe { factory.CreatePathGeometry()? };
        let sink = unsafe { geometry.Open()? };
        let (first_x, first_y) = surface(0.0);
        unsafe {
            sink.BeginFigure(Vector2::new(first_x, first_y - thickness(0.0)), D2D1_FIGURE_BEGIN_FILLED);
            for step in 1..=SEGMENTS {
                let fraction = step as f32 / SEGMENTS as f32;
                let (x, y) = surface(fraction);
                sink.AddLine(Vector2::new(x, y - thickness(fraction)));
            }
            for step in (0..=SEGMENTS).rev() {
                let fraction = step as f32 / SEGMENTS as f32;
                let (x, y) = surface(fraction);
                sink.AddLine(Vector2::new(x, y + thickness(fraction)));
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
            target.FillGeometry(&geometry, fill_brush, None::<&ID2D1Brush>);
        }

        // 同源曲线再加一条极细高光，只勾浪峰，形成前后层次而不产生粒子噪声。
        let (first_x, first_y) = surface(0.0);
        let mut previous = Vector2::new(first_x, first_y - thickness(0.0) * 0.42);
        for step in 1..=SEGMENTS {
            let fraction = step as f32 / SEGMENTS as f32;
            let (x, y) = surface(fraction);
            let next = Vector2::new(x, y - thickness(fraction) * 0.42);
            unsafe { target.DrawLine(previous, next, crest_brush, 0.42, None) };
            previous = next;
        }
    }
    Ok(())
}

/// 在已消耗填充中绘制从左向右翻滚的流光带。每一带是一片有厚度的竖向流体：
/// 它从左端进入、向当前额度前沿推进，经过右侧后再次从左端生成。因此不是简单
/// 的平移直线，更不是此前误做的自上而下条纹。
fn draw_flow_rolls(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
    band_thickness: f32,
) -> Result<(), Error> {
    let consumed_width = (bounds.right - bounds.left) * (100.0 - remaining_percent.clamp(0.0, 100.0)) / 100.0;
    if consumed_width < 4.0 {
        return Ok(());
    }
    // 仅保留三股稀疏、缓慢的水体。五股贯穿全高的发光带在任务栏小尺寸下会
    // 叠成机械竖条，这正是此前“海浪很奇怪”的主要视觉原因。
    const BANDS: usize = 3;
    const SEGMENTS: usize = 48;
    let wave = core::f32::consts::TAU;
    for band in 0..BANDS {
        // 速度为从左到右；每轮会有一片新流体在左边出现。不同 band 的卷曲相位
        // 不同，避免看上去像若干平行的竖向扫描线。
        // 每股浪有独立但始终单调向前的速度扰动：既会偶尔加快/放慢，也不会在
        // `.fract()` 的周期边界产生倒退或跳变。
        // 约 13 秒横穿填充区，接近真实海浪的缓慢推进。波带越过右侧时从左端
        // 自然重新出现，且在胶囊圆头处被裁切，不会产生可见回跳。
        let horizontal_fraction = fluid_wave_position(band, phase);
        let roll_point = |y_fraction: f32, side: f32| {
            let y = bounds.top + (bounds.bottom - bounds.top) * y_fraction;
            let (left, _) = capsule_horizontal_bounds(bounds, y);
            let edge = flow_boundary_point(bounds, remaining_percent, phase, amplitude, y_fraction);
            let available = (edge.X - left).max(0.0);
            // 多组互不整除的低频扰动形成“随机”的浪缘，但所有项均为连续函数，
            // 所以 60 FPS 下不会跳帧或在短周期归零时突然改形。
            let curl = (y_fraction * wave * 1.61 + band as f32 * 1.07 - phase * 0.58).sin() * (0.72 + amplitude * 0.19);
            let crest = (y_fraction * wave * 3.87 + band as f32 * 0.71 + phase * 0.24).sin() * 0.43
                + (y_fraction * wave * 6.73 - phase * 0.11 + band as f32 * 0.37).sin() * 0.18;
            // 每股浪的厚度也沿纵向连续变化，模拟水中高低不一的浪峰，而非全高
            // 等宽的扫描带。最小厚度受限，避免两侧在交叉时出现尖刺。
            let local_thickness = band_thickness
                * (0.72
                    + 0.22 * (y_fraction * wave * 1.19 + band as f32 * 0.83 - phase * 0.27).sin()
                    + 0.11 * (y_fraction * wave * 4.17 - band as f32 * 0.51 + phase * 0.13).sin())
                .clamp(0.34, 1.18);
            let x = (left + available * horizontal_fraction + curl + crest + side * local_thickness / 2.0)
                .clamp(left + 0.45, edge.X - 0.45);
            Vector2::new(x, y)
        };
        // 一条“浪”是有厚度的发光流体面而非一根线：两条卷曲边界合成闭合面，
        // 三层不同厚度的半透明面形成进度条内部连续流光。
        let factory = unsafe { target.GetFactory()? };
        let geometry = unsafe { factory.CreatePathGeometry()? };
        let sink = unsafe { geometry.Open()? };
        unsafe {
            sink.BeginFigure(roll_point(0.0, -1.0), D2D1_FIGURE_BEGIN_FILLED);
            for segment in 1..=SEGMENTS {
                let y_fraction = segment as f32 / SEGMENTS as f32;
                sink.AddLine(roll_point(y_fraction, -1.0));
            }
            for segment in (0..=SEGMENTS).rev() {
                let y_fraction = segment as f32 / SEGMENTS as f32;
                sink.AddLine(roll_point(y_fraction, 1.0));
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
            target.FillGeometry(&geometry, brush, None::<&ID2D1Brush>);
        }
    }
    Ok(())
}

/// 三股浪的连续横向位置。相位扰动只占基础速度的一小部分，因此波浪会自然地
/// 有快有慢，却始终从左向右推进；每股浪的进入时刻也不同，不会同时拍岸。
fn fluid_wave_position(band: usize, phase: f32) -> f32 {
    const SPEEDS: [f32; 3] = [0.067, 0.074, 0.081];
    const OFFSETS: [f32; 3] = [0.04, 0.38, 0.71];
    const DRIFT_PHASES: [f32; 3] = [0.37, 2.11, 4.49];
    let index = band % SPEEDS.len();
    let drift = 0.036 * (phase * 0.19 + DRIFT_PHASES[index]).sin();
    (OFFSETS[index] + phase * SPEEDS[index] + drift).fract()
}

/// 连续填充边界左侧的已消耗区域。`remaining_percent = 100` 时路径退到左端，
/// `remaining_percent = 0` 时铺满整个胶囊，正好符合用户定义的消耗语义。
fn draw_flow_fill(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let left_point = |step: usize| {
        let y = bounds.top + (bounds.bottom - bounds.top) * step as f32 / SEGMENTS as f32;
        Vector2::new(capsule_horizontal_bounds(bounds, y).0, y)
    };
    let edge_point =
        |step: usize| flow_boundary_point(bounds, remaining_percent, phase, amplitude, step as f32 / SEGMENTS as f32);
    let factory = unsafe { target.GetFactory()? };
    let geometry = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(left_point(0), D2D1_FIGURE_BEGIN_FILLED);
        for step in 1..=SEGMENTS {
            sink.AddLine(left_point(step));
        }
        for step in (0..=SEGMENTS).rev() {
            sink.AddLine(edge_point(step));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, brush, None::<&ID2D1Brush>);
    }
    Ok(())
}

/// 描绘一条沿 Y 方向采样的连续边界。相邻点共享同一流体公式，因此亮边随时间
/// 平滑推进而不会呈现锯齿、闪电或逐条跳动。
fn draw_flow_boundary(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
    stroke_width: f32,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let mut previous = flow_boundary_point(bounds, remaining_percent, phase, amplitude, 0.0);
    for step in 1..=SEGMENTS {
        let next = flow_boundary_point(bounds, remaining_percent, phase, amplitude, step as f32 / SEGMENTS as f32);
        unsafe { target.DrawLine(previous, next, brush, stroke_width, None) };
        previous = next;
    }
    Ok(())
}

/// 绘制拍到额度前沿之外、随即收回的淡色薄水层。此层只表达动态观感，不参与
/// 已消耗宽度计算；真实进度仍由其后的 `draw_flow_boundary` 明确锚定。
fn draw_shoreline_wash(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    wash_brush: &ID2D1SolidColorBrush,
    foam_brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let base =
        |step: usize| flow_boundary_point(bounds, remaining_percent, phase, amplitude, step as f32 / SEGMENTS as f32);
    let wash =
        |step: usize| shoreline_wash_point(bounds, remaining_percent, phase, amplitude, step as f32 / SEGMENTS as f32);
    let factory = unsafe { target.GetFactory()? };
    let geometry = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(base(0), D2D1_FIGURE_BEGIN_FILLED);
        for step in 1..=SEGMENTS {
            sink.AddLine(base(step));
        }
        for step in (0..=SEGMENTS).rev() {
            sink.AddLine(wash(step));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, wash_brush, None::<&ID2D1Brush>);
    }
    draw_shoreline_foam(target, bounds, remaining_percent, phase, amplitude, foam_brush)?;
    draw_shoreline_foam_clusters(target, bounds, remaining_percent, phase, amplitude, foam_brush)?;
    Ok(())
}

/// 用细而连续的亮线勾出薄水层最外缘。它与半透明外溅使用相同的连续方程，
/// 所以浪扑出和回收时会保持黏连，不会像额外叠了一条独立的闪光进度线。
fn draw_shoreline_foam(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    const SEGMENTS: usize = 80;
    let mut previous = shoreline_wash_point(bounds, remaining_percent, phase, amplitude, 0.0);
    let mut previous_strength = shoreline_splash_strength(phase);
    for step in 1..=SEGMENTS {
        let y_fraction = step as f32 / SEGMENTS as f32;
        let next = shoreline_wash_point(bounds, remaining_percent, phase, amplitude, y_fraction);
        let next_strength = shoreline_splash_strength(phase);
        // 水花完全收回时不保留常亮竖线；只在外溅存在的片段绘制不规则泡沫。
        let foam_strength = previous_strength.min(next_strength) * shoreline_local_shape(phase, y_fraction);
        if foam_strength > 0.035 {
            unsafe { target.DrawLine(previous, next, brush, 0.42 + foam_strength * 0.86, None) };
        }
        previous = next;
        previous_strength = next_strength;
    }
    Ok(())
}

/// 破碎浪花不是散落到整条进度条的粒子，而是严格贴在薄水层里的少量泡沫簇。
/// 它们只会随抵岸主浪出现、沿外溅方向滑行并在退潮时消失，补足“破碎”的质感。
fn draw_shoreline_foam_clusters(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let impact = shoreline_splash_strength(phase);
    if impact < 0.16 {
        return Ok(());
    }
    for cluster in 0..4 {
        let y_fraction = 0.17 + cluster as f32 * 0.22;
        let base = flow_boundary_point(bounds, remaining_percent, phase, amplitude, y_fraction);
        let wash = shoreline_wash_point(bounds, remaining_percent, phase, amplitude, y_fraction);
        let extension = (wash.X - base.X).max(0.0);
        if extension < 0.7 {
            continue;
        }
        let drift = 0.24 + 0.57 * (0.5 + 0.5 * (phase * 0.83 + cluster as f32 * 1.71).sin());
        let x = base.X + extension * drift;
        let y = base.Y + (phase * 0.61 + cluster as f32 * 2.19).sin() * (0.34 + impact * 0.38);
        let radius = (0.28 + impact * 0.68) * (0.78 + 0.20 * (phase * 0.43 + cluster as f32).sin()).abs();
        unsafe {
            target.FillEllipse(
                &D2D1_ELLIPSE { point: Vector2::new(x, y), radiusX: radius, radiusY: radius * 0.72 },
                brush,
            );
        }
    }
    Ok(())
}

/// 根据当前从左推进的波带，计算“拍岸”后短暂越过真实额度边沿的淡色薄层。
/// 每个波带只在接近前沿时触发；多组低频扰动控制水花的局部形状，因此既有
/// 随机感又完全连续。影响力归零时该点自然回到真实边缘。
fn shoreline_wash_point(
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    y_fraction: f32,
) -> Vector2 {
    let base = flow_boundary_point(bounds, remaining_percent, phase, amplitude, y_fraction);
    let impact = shoreline_splash_strength(phase);
    let local_shape = shoreline_local_shape(phase, y_fraction);
    let (_, right) = capsule_horizontal_bounds(bounds, base.Y);
    // 5--8 DIP 的淡色外溅在高 DPI 任务栏中仍清楚可见。`impact` 严格遵循
    // 推进→破碎铺展→慢速退潮三个阶段；它不会改变真实额度边界或百分比语义。
    let extension = impact * local_shape * (2.75 + amplitude * 2.15);
    Vector2::new((base.X + extension).min(right), base.Y)
}

/// 唯一一股主浪抵达额度前沿后的连续“拍岸”生命周期。
///
/// - 0.90 前：主浪仍在水体内推进，绝不提前打到岸上；
/// - 0.90–0.925：浪峰实际抵达边沿后破碎并快速把薄水推到沙滩；
/// - 0.925–0.998：薄水与泡沫缓慢退回；
/// - 重置后：下一股浪从左端重新形成。
///
/// 此函数只控制视觉外溅，额度的实际边界从不后退。
fn shoreline_splash_strength(phase: f32) -> f32 {
    // 只有最前方的主浪可拍岸。其它两层只负责水体的深浅/高低层次，绝不能
    // 同时制造多个外溅事件，否则视觉上会像三根曲线一起撞到边界。
    shoreline_splash_strength_for_progress(fluid_wave_position(0, phase))
}

/// 单股浪的拍岸强度，拆出为纯函数以固定“快速铺展、较慢回退”的动画语义。
fn shoreline_splash_strength_for_progress(progress: f32) -> f32 {
    if !(0.90..0.998).contains(&progress) {
        0.0
    } else if progress < 0.925 {
        smooth_step(0.90, 0.925, progress)
    } else {
        1.0 - smooth_step(0.925, 0.998, progress)
    }
}

/// 同一股拍岸浪在纵向上有高低、宽窄和缓慢漂移；互不整除的连续频率使它自然
/// 不规则，而不是固定重复的波纹或锯齿。
fn shoreline_local_shape(phase: f32, y_fraction: f32) -> f32 {
    let wave = core::f32::consts::TAU;
    (0.62
        + 0.24 * (y_fraction * wave * 1.31 - phase * 0.33).sin()
        + 0.14 * (y_fraction * wave * 3.83 + phase * 0.17 + 0.91).sin()
        + 0.08 * (y_fraction * wave * 7.07 - phase * 0.07 + 2.13).sin())
    .clamp(0.12, 1.0)
}

/// 平滑插值的有限区间版本，避免手写线性分段在切换点造成可见速度突变。
fn smooth_step(start: f32, end: f32, value: f32) -> f32 {
    let t = ((value - start) / (end - start)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 计算沙滩浪线风格边界的单个采样点。基础 X 由已消耗比例决定；互不整除的
/// 低频连续扰动模拟非重复的自然涨落，但不改变总体额度宽度。
fn flow_boundary_point(
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    y_fraction: f32,
) -> Vector2 {
    let y_fraction = y_fraction.clamp(0.0, 1.0);
    let y = bounds.top + (bounds.bottom - bounds.top) * y_fraction;
    let wave = core::f32::consts::TAU;
    let irregular = (y_fraction * wave * 1.43 + phase * 0.34).sin() * amplitude * 0.62
        + (y_fraction * wave * 3.71 - phase * 0.19 + 0.83).sin() * amplitude * 0.28
        + (y_fraction * wave * 6.89 + phase * 0.11 + 1.47).sin() * amplitude * 0.12
        + (y_fraction * wave * 10.37 - phase * 0.047 + 2.11).sin() * amplitude * 0.05;
    let (left, right) = capsule_horizontal_bounds(bounds, y);
    let x = (flow_boundary_base_x(bounds, remaining_percent) + irregular).clamp(left, right);
    Vector2::new(x, y)
}

/// 将剩余额度转换成填充左边界的“已消耗”宽度。这个独立函数供测试使用，防止
/// 未来重构时再次误把剩余百分比直接映射为填充量。
fn flow_boundary_base_x(bounds: D2D_RECT_F, remaining_percent: f32) -> f32 {
    let consumed = (100.0 - remaining_percent.clamp(0.0, 100.0)) / 100.0;
    bounds.left + (bounds.right - bounds.left) * consumed
}

/// 返回椭圆胶囊在指定 y 处可绘制的横向范围，使流式边界和左侧填充在圆头处
/// 同步收束，不会穿出透明圆角。
fn capsule_horizontal_bounds(bounds: D2D_RECT_F, y: f32) -> (f32, f32) {
    let height = (bounds.bottom - bounds.top).max(1.0);
    let radius = (height / 2.0).min((bounds.right - bounds.left).max(1.0) / 2.0);
    let center_y = (bounds.top + bounds.bottom) / 2.0;
    let vertical = (y - center_y).abs().min(radius);
    let half = (radius * radius - vertical * vertical).max(0.0).sqrt();
    (bounds.left + radius - half, bounds.right - radius + half)
}

/// 用一个闭合 Direct2D 几何路径连续填充波面。`remaining_percent` 只决定水面
/// 高度；波形只负责动态质感，因此不会把没有额度误画成“有进度”。
///
/// 这里不能用逐条 `DrawLine` 近似填充：Layered Window 缩放到实际任务栏高度后，
/// 半透明竖线会形成明显栅格条纹。单一闭合路径可让 Direct2D 一次完成抗锯齿填充。
fn draw_wave_fill(
    target: &ID2D1RenderTarget,
    bounds: D2D_RECT_F,
    remaining_percent: f32,
    phase: f32,
    amplitude: f32,
    cycles: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let width = (bounds.right - bounds.left).max(1.0);
    let height = (bounds.bottom - bounds.top).max(1.0);
    // 水位表达“剩余”，与任务栏右侧百分比、设计稿和官方额度语义保持一致：
    // 0% 时水线贴近底部，100% 时水面升到顶部。此前按“已消耗”反向填充，
    // 会让 5h 0% 在画面中反而变成整条紫色海洋。
    let remaining = remaining_percent.clamp(0.0, 100.0) / 100.0;
    let baseline = bounds.bottom - remaining * height;
    const SEGMENTS: usize = 192;
    let surface_point = |step: usize| {
        let t = step as f32 / SEGMENTS as f32;
        let x = bounds.left + t * width;
        let angle = core::f32::consts::TAU * (t * cycles + phase);
        let harmonic = (angle * 1.93 + 0.7).sin() * amplitude * 0.32;
        let (capsule_top, capsule_bottom) = capsule_vertical_bounds(bounds, x);
        let y = (baseline + angle.sin() * amplitude + harmonic).clamp(capsule_top, capsule_bottom);
        Vector2::new(x, y)
    };
    let bottom_point = |step: usize| {
        let t = step as f32 / SEGMENTS as f32;
        let x = bounds.left + t * width;
        let (_, bottom) = capsule_vertical_bounds(bounds, x);
        Vector2::new(x, bottom)
    };
    let factory = unsafe { target.GetFactory()? };
    let geometry = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(bottom_point(0), D2D1_FIGURE_BEGIN_FILLED);
        for step in 0..=SEGMENTS {
            sink.AddLine(surface_point(step));
        }
        for step in (0..=SEGMENTS).rev() {
            sink.AddLine(bottom_point(step));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        target.FillGeometry(&geometry, brush, None::<&ID2D1Brush>);
    }
    Ok(())
}

/// 返回椭圆胶囊在指定 x 位置的可绘制纵向范围。使用几何裁切而不是纯矩形
/// `PushAxisAlignedClip`，确保填充、轮廓和浪花在两端一起顺着圆角收束。
fn capsule_vertical_bounds(bounds: D2D_RECT_F, x: f32) -> (f32, f32) {
    let height = (bounds.bottom - bounds.top).max(1.0);
    let radius = (height / 2.0).min((bounds.right - bounds.left).max(1.0) / 2.0);
    let center_y = (bounds.top + bounds.bottom) / 2.0;
    let center_x = if x < bounds.left + radius {
        bounds.left + radius
    } else if x > bounds.right - radius {
        bounds.right - radius
    } else {
        return (bounds.top, bounds.bottom);
    };
    let horizontal = (x - center_x).abs().min(radius);
    let half = (radius * radius - horizontal * horizontal).max(0.0).sqrt();
    (center_y - half, center_y + half)
}

/// 将 `缓存 … · Credits …` 分为设计稿中的左下与中部两个独立锚点；未满足
/// Credits 显示条件时不会凭空留出占位。
fn split_taskbar_secondary(value: &str) -> (String, Option<String>) {
    let Some((cache, credits)) = value.split_once(" · Credits ") else { return (value.to_owned(), None) };
    (cache.to_owned(), Some(format!("Credits {credits}")))
}

fn fluid_foreground_color(state: codex_taskbar_domain::activity::ActivityState) -> D2D1_COLOR_F {
    use codex_taskbar_domain::activity::ActivityState;

    match state {
        ActivityState::Executing => color(0.10, 0.72, 1.0, 0.80),
        ActivityState::Thinking | ActivityState::Reviewing => color(0.38, 0.42, 1.0, 0.76),
        ActivityState::WaitingForUser => color(0.94, 0.62, 0.18, 0.74),
        ActivityState::Idle | ActivityState::Completed => color(0.18, 0.78, 0.46, 0.70),
        ActivityState::Failed => color(0.92, 0.25, 0.30, 0.66),
        ActivityState::Unknown => color(0.31, 0.47, 0.66, 0.62),
    }
}

/// 前景浪的水线/泡沫高光与状态色同源，但亮度更高，避免状态变成绿色或琥珀后
/// 仍残留固定青色水线，破坏“颜色反映活动状态”的语义。
fn fluid_highlight_color(state: codex_taskbar_domain::activity::ActivityState) -> D2D1_COLOR_F {
    use codex_taskbar_domain::activity::ActivityState;

    match state {
        ActivityState::Executing => color(0.42, 0.96, 1.0, 0.90),
        ActivityState::Thinking | ActivityState::Reviewing => color(0.63, 0.69, 1.0, 0.88),
        ActivityState::WaitingForUser => color(1.0, 0.78, 0.35, 0.88),
        ActivityState::Idle | ActivityState::Completed => color(0.46, 0.96, 0.66, 0.88),
        ActivityState::Failed => color(1.0, 0.52, 0.62, 0.88),
        ActivityState::Unknown => color(0.57, 0.71, 0.88, 0.80),
    }
}

#[allow(dead_code)]
fn draw_center_percent(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    weekly: RingArc,
    brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let (number, percent) = center_percent_parts(weekly.sweep_angle);
    let hole_radius = (weekly.radius - weekly.stroke_width - 0.75).max(7.0);
    let layout = circle_bounds_rect(weekly.center, hole_radius);
    let max_width = hole_radius * 2.0;
    let max_height = max_width;

    if percent.is_none() {
        let format = create_text_format(
            factory,
            9.75,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        )?;
        draw_text_line(target, &format, brush, &layout, &number);
        return Ok(());
    }

    // 数字与百分号使用两个 text run：百分号更小并略微上移。每轮都用 DirectWrite
    // 实际测量宽度，100% 或字体回退变宽时同步缩小，保证完整留在圆环孔径内。
    let mut number_size = if number.len() >= 3 { 8.75 } else { 10.25 };
    let mut percent_size = if number.len() >= 3 { 5.5 } else { 6.25 };
    loop {
        let number_format = create_text_format(
            factory,
            number_size,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        )?;
        let percent_format = create_text_format(
            factory,
            percent_size,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        )?;
        let number_width = measure_text_width(factory, &number_format, &number, max_width, max_height)?;
        let percent_width = measure_text_width(factory, &percent_format, "%", max_width, max_height)?;
        let gap = 0.35;
        let total_width = number_width + gap + percent_width;
        if total_width <= max_width - 0.5 || number_size <= 7.0 {
            let left = weekly.center.x - total_width / 2.0;
            let number_rect = rect(left, layout.top, left + number_width + 0.5, layout.bottom);
            let percent_rect =
                rect(left + number_width + gap, layout.top - 1.35, left + total_width + 0.5, layout.bottom - 1.35);
            draw_text_line(target, &number_format, brush, &number_rect, &number);
            draw_text_line(target, &percent_format, brush, &percent_rect, "%");
            break;
        }
        number_size -= 0.25;
        percent_size = (percent_size - 0.15).max(4.75);
    }
    Ok(())
}

#[allow(dead_code)]
fn center_percent_parts(sweep_angle: Option<f32>) -> (String, Option<&'static str>) {
    match sweep_angle {
        Some(sweep) => {
            let percent = ((sweep / core::f32::consts::TAU) * 100.0).round().clamp(0.0, 100.0) as i32;
            (percent.to_string(), Some("%"))
        }
        None => ("--".to_owned(), None),
    }
}

fn measure_text_width(
    factory: &IDWriteFactory,
    format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    text: &str,
    max_width: f32,
    max_height: f32,
) -> Result<f32, Error> {
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    let text_layout = unsafe { factory.CreateTextLayout(&utf16, format, max_width, max_height)? };
    let mut metrics = DWRITE_TEXT_METRICS::default();
    unsafe { text_layout.GetMetrics(&mut metrics)? };
    Ok(metrics.widthIncludingTrailingWhitespace)
}

#[allow(dead_code)]
fn draw_right_summary(
    factory: &IDWriteFactory,
    target: &ID2D1RenderTarget,
    surface_size: (f32, f32),
    summary_left: f32,
    details: &NativeHostDetails,
    primary_brush: &ID2D1SolidColorBrush,
    secondary_brush: &ID2D1SolidColorBrush,
) -> Result<(), Error> {
    let (line1, line2) = summary_lines(details);
    let left = summary_left.min(surface_size.0 - 8.0);
    let right = (surface_size.0 - 4.0).max(left + 8.0);
    let total_height = 33.0_f32.min(surface_size.1);
    let top = ((surface_size.1 - total_height) / 2.0).max(0.0);
    let line1_height = (total_height * 0.57).max(16.0);
    let line1_rect = D2D_RECT_F { left, top, right, bottom: top + line1_height };
    let line2_rect = D2D_RECT_F { left, top: top + line1_height, right, bottom: top + total_height };
    let line1_format = create_text_format(
        factory,
        13.5,
        DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    )?;
    let line2_format = create_text_format(
        factory,
        11.25,
        DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    )?;
    draw_text_line(target, &line1_format, primary_brush, &line1_rect, &line1);
    draw_text_line(target, &line2_format, secondary_brush, &line2_rect, &line2);
    Ok(())
}

fn create_text_format(
    factory: &IDWriteFactory,
    font_size: f32,
    weight: windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT,
    text_alignment: windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT,
    paragraph_alignment: windows::Win32::Graphics::DirectWrite::DWRITE_PARAGRAPH_ALIGNMENT,
) -> Result<windows::Win32::Graphics::DirectWrite::IDWriteTextFormat, Error> {
    let format = unsafe {
        factory.CreateTextFormat(
            w!("Segoe UI Variable Text"),
            None::<&IDWriteFontCollection>,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            font_size,
            w!("zh-CN"),
        )?
    };
    unsafe {
        format.SetTextAlignment(text_alignment)?;
        format.SetParagraphAlignment(paragraph_alignment)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    }
    Ok(format)
}

fn draw_text_line(
    target: &ID2D1RenderTarget,
    format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    brush: &ID2D1SolidColorBrush,
    rect: &D2D_RECT_F,
    text: &str,
) {
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    unsafe {
        target.DrawText(&utf16, format, rect, brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }
}

fn summary_lines(details: &NativeHostDetails) -> (String, String) {
    if details.summary_lines[0].is_some() || details.summary_lines[1].is_some() {
        return (
            details.summary_lines[0].clone().unwrap_or_else(|| "今日 --".to_owned()),
            details.summary_lines[1].clone().unwrap_or_else(|| "缓存 --".to_owned()),
        );
    }
    // 任务栏不再展示活动状态文字或“当前任务 Token”。当生产端尚未给出结构化
    // 摘要时，使用中性的业务占位而非暴露旧胶囊的技术性“未知状态 / 暂无数据”。
    ("今日 --".to_owned(), "缓存 --".to_owned())
}

fn format_compact_number(value: u64) -> String {
    // 统一按一位小数四舍五入，并在 999.5K/999.5M 处进位；趋势图图例、
    // Token 构成图和任务栏摘要因此不会分别显示 999.4K、1000K 等边界值。
    if value >= 1_000_000 {
        return format_compact_scaled(value, 1_000_000, 'M');
    }
    if value >= 1_000 {
        if value >= 999_500 {
            return "1M".to_owned();
        }
        return format_compact_scaled(value, 1_000, 'K');
    }
    value.to_string()
}

fn format_compact_scaled(value: u64, unit: u64, suffix: char) -> String {
    let tenths = ((value % unit).saturating_mul(10).saturating_add(unit / 2)) / unit;
    let mut major = value / unit;
    if tenths >= 10 {
        major = major.saturating_add(1);
        return format!("{major}{suffix}");
    }
    if tenths == 0 { format!("{major}{suffix}") } else { format!("{major}.{tenths}{suffix}") }
}

#[allow(dead_code)]
fn circle_bounds_rect(center: crate::render_model::DipPoint, radius: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: center.x - radius, top: center.y - radius, right: center.x + radius, bottom: center.y + radius }
}

#[allow(dead_code)]
fn ellipse(circle: Circle) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE {
        point: Vector2::new(circle.center.x, circle.center.y),
        radiusX: circle.radius,
        radiusY: circle.radius,
    }
}

const fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

#[allow(dead_code)]
const fn lamp_color(tone: LampColor) -> D2D1_COLOR_F {
    match tone {
        LampColor::Green => color(0.25, 0.91, 0.49, 1.0),
        LampColor::Cyan => color(0.17, 0.82, 0.95, 1.0),
        LampColor::BlueViolet => color(0.55, 0.39, 0.98, 1.0),
        LampColor::Amber => color(1.0, 0.68, 0.18, 1.0),
        LampColor::Red => color(0.95, 0.27, 0.30, 1.0),
        LampColor::Gray => color(0.56, 0.59, 0.64, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_number_formats_token_values() {
        assert_eq!(format_compact_number(999), "999");
        assert_eq!(format_compact_number(1_200), "1.2K");
        assert_eq!(format_compact_number(12_000), "12K");
        assert_eq!(format_compact_number(1_500_000), "1.5M");
        assert_eq!(format_compact_number(999_499), "999.5K");
        assert_eq!(format_compact_number(999_500), "1M");
        assert_eq!(format_compact_number(1_999_500), "2M");
    }

    #[test]
    fn capsule_vertical_bounds_follow_the_elliptical_end_caps() {
        let bounds = rect(0.0, 0.0, 120.0, 40.0);
        assert_eq!(capsule_vertical_bounds(bounds, 60.0), (0.0, 40.0));
        let (left_top, left_bottom) = capsule_vertical_bounds(bounds, 0.0);
        assert!((left_top - 20.0).abs() < f32::EPSILON);
        assert!((left_bottom - 20.0).abs() < f32::EPSILON);
        let (near_top, near_bottom) = capsule_vertical_bounds(bounds, 10.0);
        assert!(near_top > 0.0 && near_bottom < 40.0);
    }

    #[test]
    fn capsule_horizontal_bounds_follow_the_elliptical_end_caps() {
        let bounds = rect(0.0, 0.0, 120.0, 40.0);
        assert_eq!(capsule_horizontal_bounds(bounds, 20.0), (0.0, 120.0));
        let (top_left, top_right) = capsule_horizontal_bounds(bounds, 0.0);
        assert!((top_left - 20.0).abs() < f32::EPSILON);
        assert!((top_right - 100.0).abs() < f32::EPSILON);
        let (near_left, near_right) = capsule_horizontal_bounds(bounds, 10.0);
        assert!(near_left > 0.0 && near_right < 120.0);
    }

    #[test]
    fn flow_width_represents_consumed_not_remaining_quota() {
        let bounds = rect(10.0, 0.0, 210.0, 40.0);
        assert_eq!(flow_boundary_base_x(bounds, 100.0), 10.0);
        assert_eq!(flow_boundary_base_x(bounds, 50.0), 110.0);
        assert_eq!(flow_boundary_base_x(bounds, 0.0), 210.0);
    }

    #[test]
    fn shore_splash_has_a_push_and_a_longer_continuous_return() {
        // 直接验证一股浪的生命周期：前沿前没有外溅，破碎阶段快速上升，随后在
        // 更长的退潮区间连续回落，不能在周期重置时留下孤立亮线。
        assert_eq!(shoreline_splash_strength_for_progress(0.89), 0.0);
        assert!(shoreline_splash_strength_for_progress(0.92) > 0.5);
        assert!(shoreline_splash_strength_for_progress(0.94) > shoreline_splash_strength_for_progress(0.98));
        assert!(shoreline_splash_strength_for_progress(0.98) > 0.0);
        assert_eq!(shoreline_splash_strength_for_progress(0.999), 0.0);
    }

    #[test]
    fn fluid_highlight_changes_with_activity_semantics() {
        use codex_taskbar_domain::activity::ActivityState;

        assert_ne!(fluid_highlight_color(ActivityState::Executing), fluid_highlight_color(ActivityState::Idle));
        assert_ne!(fluid_highlight_color(ActivityState::Thinking), fluid_highlight_color(ActivityState::Failed));
    }

    #[test]
    fn popup_layout_transform_scales_down_without_distortion_or_upscaling() {
        let identity = fit_layout_transform(DETAILS_LAYOUT_SIZE, DETAILS_LAYOUT_SIZE);
        assert_eq!(identity, identity_transform());

        let height_limited =
            fit_layout_transform((DETAILS_LAYOUT_SIZE.0, DETAILS_LAYOUT_SIZE.1 * 0.8), DETAILS_LAYOUT_SIZE);
        assert!((height_limited.M11 - 0.8).abs() < 0.001);
        assert_eq!(height_limited.M11, height_limited.M22);
        assert!((height_limited.M31 - 96.0).abs() < 0.01);
        assert_eq!(height_limited.M32, 0.0);

        let larger_surface = fit_layout_transform((1_100.0, 760.0), DETAILS_LAYOUT_SIZE);
        assert_eq!(larger_surface.M11, 1.0);
        assert_eq!(larger_surface.M22, 1.0);
        assert_eq!(larger_surface.M31, 70.0);
        assert_eq!(larger_surface.M32, 50.0);
    }

    #[test]
    fn official_account_column_keeps_about_one_quarter_of_content_width() {
        let width = compact_official_left_width(828.0);
        assert!((width - 215.28).abs() < 0.01);
        assert!(width < 220.0);
        assert_eq!(compact_official_left_width(400.0), 210.0);
        assert_eq!(compact_official_left_width(1_200.0), 226.0);
    }

    #[test]
    fn official_detail_rows_split_at_section_without_copying_values() {
        let rows = vec![
            crate::host::NativeDetailRow::new("输入", "24.8K"),
            crate::host::NativeDetailRow::new("输出", "6.6K"),
            crate::host::NativeDetailRow::section("账户活动"),
            crate::host::NativeDetailRow::new("账户累计", "12.7M"),
            crate::host::NativeDetailRow::new("单日峰值", "486.2K"),
        ];
        let (task, account) = split_official_detail_rows(&rows);
        assert_eq!(task.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(), ["输入", "输出"]);
        assert_eq!(account.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(), ["账户累计", "单日峰值"]);
    }

    #[test]
    fn trend_smoothing_keeps_real_endpoints_and_never_overshoots_segment_values() {
        let anchors = [Vector2::new(0.0, 10.0), Vector2::new(20.0, 2.0), Vector2::new(40.0, 8.0)];
        let curve = monotone_cubic_curve(&anchors, 8);
        assert_eq!(curve.first(), anchors.first());
        assert_eq!(curve.last(), anchors.last());
        assert_eq!(curve.len(), 17);
        assert_eq!(curve[8], anchors[1]);
        assert!(curve[..=8].iter().all(|point| (2.0..=10.0).contains(&point.Y)));
        assert!(curve[8..].iter().all(|point| (2.0..=8.0).contains(&point.Y)));
    }

    #[test]
    fn trend_smoothing_preserves_flat_sections_without_ringing() {
        let anchors =
            [Vector2::new(0.0, 8.0), Vector2::new(20.0, 8.0), Vector2::new(40.0, 8.0), Vector2::new(60.0, 2.0)];
        let curve = monotone_cubic_curve(&anchors, 12);
        assert!(curve[..=24].iter().all(|point| (point.Y - 8.0).abs() < f32::EPSILON));
        assert!(curve[24..].iter().all(|point| (2.0..=8.0).contains(&point.Y)));
    }

    #[test]
    fn grouped_token_number_is_exact_for_hover_tooltip() {
        assert_eq!(format_grouped_number(0), "0");
        assert_eq!(format_grouped_number(999), "999");
        assert_eq!(format_grouped_number(1_000), "1,000");
        assert_eq!(format_grouped_number(12_784_320), "12,784,320");
    }

    #[test]
    fn trend_hit_test_maps_high_dpi_mouse_to_nearest_real_bucket() {
        let details = NativeHostDetails {
            compact_primary_column: true,
            trend_points: (0..5)
                .map(|index| crate::host::NativeTrendPoint::new(format!("8/{}", index + 18), index as u64))
                .collect(),
            ..NativeHostDetails::default()
        };
        let bounds = compact_official_trend_bounds(&details).expect("trend bounds");
        let plot = trend_plot_bounds(bounds);
        let plot_left = plot.left;
        let plot_right = plot.right;
        let third_x = plot_left + (plot_right - plot_left) / 2.0;
        let y = (plot.top + plot.bottom) / 2.0;
        assert_eq!(
            details_trend_hit_test((1_200, 825), 120.0, &details, ((third_x * 1.25) as i32, (y * 1.25) as i32)),
            Some(2)
        );
        assert_eq!(details_trend_hit_test((1_200, 825), 120.0, &details, (0, 0)), None);
        let title_y = (bounds.top + 2.0) * 1.25;
        assert_eq!(
            details_trend_hit_test((1_200, 825), 120.0, &details, ((third_x * 1.25) as i32, title_y as i32)),
            None
        );
    }

    #[test]
    fn details_actions_share_stable_high_dpi_hit_regions() {
        for action in [DetailsAction::Refresh, DetailsAction::OpenSettings] {
            let bounds = details_action_bounds(action);
            let center_x = (bounds.left + bounds.right) / 2.0;
            let center_y = (bounds.top + bounds.bottom) / 2.0;
            assert_eq!(
                details_action_hit_test(
                    (1_200, 825),
                    120.0,
                    ((center_x * 1.25).round() as i32, (center_y * 1.25).round() as i32),
                ),
                Some(action)
            );
        }
        assert_eq!(details_action_hit_test((1_200, 825), 120.0, (20, 20)), None);
        assert_eq!(details_action_hit_test((0, 825), 120.0, (20, 20)), None);
    }

    #[test]
    fn center_percent_parts_cover_full_range_without_dropping_percent_sign() {
        for expected in [0, 8, 68, 99, 100] {
            let sweep = core::f32::consts::TAU * expected as f32 / 100.0;
            assert_eq!(center_percent_parts(Some(sweep)), (expected.to_string(), Some("%")));
        }
        assert_eq!(center_percent_parts(None), ("--".to_owned(), None));
    }

    #[test]
    fn summary_lines_never_fall_back_to_activity_or_current_thread_token() {
        let details = NativeHostDetails { title: "Codex Taskbar 详情".to_owned(), ..NativeHostDetails::default() };
        let details = NativeHostDetails {
            body: "活动状态: WaitingForUser\r\n本线程 Token: 12345 (App Server)\r\n5h 额度: 当前账户未提供 5h 窗口"
                .to_owned(),
            summary_lines: [None, None],
            ..details
        };
        let (line1, line2) = summary_lines(&details);
        assert_eq!(line1, "今日 --");
        assert_eq!(line2, "缓存 --");
    }

    #[test]
    fn structured_summary_lines_take_priority_over_body_fallback() {
        let details = NativeHostDetails { title: "Codex Taskbar 详情".to_owned(), ..NativeHostDetails::default() };
        let details = NativeHostDetails {
            body: "活动状态: Idle".to_owned(),
            summary_lines: [Some("执行中  T 31.4K".to_owned()), Some("5h 43%".to_owned())],
            ..details
        };
        let (line1, line2) = summary_lines(&details);
        assert_eq!(line1, "执行中  T 31.4K");
        assert_eq!(line2, "5h 43%");
    }
}

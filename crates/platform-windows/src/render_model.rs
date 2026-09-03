//! 与 Direct2D 无关的任务栏绘制模型。
//!
//! 本模块只产生 DIP（device-independent pixel）坐标、圆弧和动画计划，因而可以在
//! 非 Windows CI 中完整测试。Windows 后端只需要把这些图元映射为 Direct2D 调用。

use core::f32::consts::{FRAC_PI_2, TAU};

use codex_taskbar_domain::activity::{ActivityState, LampColor};

/// Direct2D 使用的逻辑像素（96 DPI）坐标点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DipPoint {
    pub x: f32,
    pub y: f32,
}

/// Direct2D 使用的逻辑像素矩形。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DipRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl DipRect {
    #[must_use]
    pub const fn width(self) -> f32 {
        self.right - self.left
    }

    #[must_use]
    pub const fn height(self) -> f32 {
        self.bottom - self.top
    }

    #[must_use]
    pub const fn center(self) -> DipPoint {
        DipPoint { x: (self.left + self.right) / 2.0, y: (self.top + self.bottom) / 2.0 }
    }

    #[must_use]
    pub fn inset(self, amount: f32) -> Self {
        Self {
            left: self.left + amount,
            top: self.top + amount,
            right: self.right - amount,
            bottom: self.bottom - amount,
        }
    }
}

/// 目前可用的额度进度；`Unavailable` 不画弧，只画中性边框。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProgressValue {
    Known { remaining_percent: f32 },
    Unavailable,
}

/// 5h 窗口的三态。`Absent` 与 `Unknown` 都不建立内环，但只有前者触发单环加粗布局。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FiveHourProgress {
    Present(ProgressValue),
    Absent,
    Unknown,
}

/// 圆环输入。外环恒为周额度，内环恒为 5h 额度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaRingsInput {
    pub bounds: DipRect,
    pub weekly: ProgressValue,
    pub five_hour: FiveHourProgress,
}

/// 一个可由后端描边的圆弧；角度从 12 点开始，顺时针为正。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingArc {
    pub center: DipPoint,
    pub radius: f32,
    pub stroke_width: f32,
    pub start_angle: f32,
    /// `None` 表示数据未知，只绘制中性轨道。
    pub sweep_angle: Option<f32>,
}

/// 嵌套额度环的最终布局。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaRingsModel {
    pub weekly: RingArc,
    pub five_hour: Option<RingArc>,
}

/// 生成嵌套圆环的 DIP 模型。
///
/// `Absent` 时不会留下内环占位，外环改为居中且加粗；`Unknown` 则保留标准外环，
/// 以免把暂时无数据误表现为权威的 5h 缺失。
#[must_use]
pub fn quota_rings_model(input: QuotaRingsInput) -> QuotaRingsModel {
    let center = input.bounds.center();
    let half_extent = (input.bounds.width().min(input.bounds.height()) / 2.0).max(0.0);
    let weekly_sweep = progress_sweep(input.weekly);
    match input.five_hour {
        FiveHourProgress::Absent => QuotaRingsModel {
            weekly: RingArc {
                center,
                radius: (half_extent - 2.0).max(0.0),
                stroke_width: 2.5,
                start_angle: -FRAC_PI_2,
                sweep_angle: weekly_sweep,
            },
            five_hour: None,
        },
        FiveHourProgress::Present(value) => {
            // 34 DIP 占位下得到 15 / 12.1 DIP 的两条中心线。结合 2.0 / 1.7
            // DIP 线宽，两环边缘净距约 1.05 DIP，既不会粘连，也不会像 V1 那样松散。
            let outer_radius = (half_extent - 2.0).max(0.0);
            QuotaRingsModel {
                weekly: RingArc {
                    center,
                    radius: outer_radius,
                    stroke_width: 2.0,
                    start_angle: -FRAC_PI_2,
                    sweep_angle: weekly_sweep,
                },
                five_hour: Some(RingArc {
                    center,
                    radius: (outer_radius - 2.9).max(0.0),
                    stroke_width: 1.7,
                    start_angle: -FRAC_PI_2,
                    sweep_angle: progress_sweep(value),
                }),
            }
        }
        FiveHourProgress::Unknown => QuotaRingsModel {
            weekly: RingArc {
                center,
                radius: (half_extent - 2.0).max(0.0),
                stroke_width: 2.0,
                start_angle: -FRAC_PI_2,
                sweep_angle: weekly_sweep,
            },
            five_hour: None,
        },
    }
}

/// 将服务端剩余百分比裁剪为完整圆的弧角。非有限数值按 0% 处理，避免传给 Direct2D NaN。
#[must_use]
pub fn remaining_percent_to_sweep(remaining_percent: f32) -> f32 {
    let percent = if remaining_percent.is_finite() { remaining_percent.clamp(0.0, 100.0) } else { 0.0 };
    TAU * percent / 100.0
}

fn progress_sweep(value: ProgressValue) -> Option<f32> {
    match value {
        ProgressValue::Known { remaining_percent } => Some(remaining_percent_to_sweep(remaining_percent)),
        ProgressValue::Unavailable => None,
    }
}

/// 状态灯的绘制输入。`entered_at_ms` 必须在状态每次变化时重置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityLampInput {
    pub state: ActivityState,
    pub entered_at_ms: u64,
    pub now_ms: u64,
}

/// 状态灯的核心圆和光晕圆。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    pub center: DipPoint,
    pub radius: f32,
}

/// 光晕的视觉参数。后端可采用多层透明圆或径向渐变实现。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glow {
    pub circle: Circle,
    pub opacity: f32,
}

/// 状态灯的最终模型。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivityLampModel {
    pub color: LampColor,
    pub core: Circle,
    pub glow: Option<Glow>,
    /// 仅动画期间使用，且只应失效此范围。
    pub dirty_bounds: DipRect,
}

/// 动画定时请求。正常模式恒定约 60 FPS；降低动画设置由宿主明确关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationSchedule {
    pub next_frame_at_ms: Option<u64>,
}

/// 单次绘制输入和结果。调用方仅在 `next_frame_at_ms` 存在时失效流式胶囊脏区。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderModel {
    pub rings: QuotaRingsModel,
    pub lamp: ActivityLampModel,
    /// V2 任务栏的主视觉：同一水域内的 Weekly 前景浪与 5h 底浪。
    ///
    /// 保留 `rings`/`lamp` 仅为兼容旧的纯模型测试；Windows 渲染器不再绘制它们。
    pub fluid: FluidQuotaModel,
    pub animation: AnimationSchedule,
    /// 设置可以完全隐藏额度环；渲染层不得继续留下中心百分比或透明占位。
    pub show_quota: bool,
    /// 隐藏状态灯时同时停止其动画调度，避免不可见组件仍消耗空闲 CPU。
    pub show_lamp: bool,
    /// 右侧双行摘要的起始 DIP，由可见视觉组件及其顺序共同决定。
    pub summary_left: f32,
    /// 未消耗区域的深色玻璃不透明度，由设置页控制。
    pub taskbar_background_opacity: f32,
}

/// 单个任务栏胶囊中的叠浪进度模型。两层浪共享 `bounds`，不能被布局层拆成上下轨道。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FluidQuotaModel {
    pub bounds: DipRect,
    /// Weekly 前景浪的剩余百分比；`None` 表示官方尚未返回可信额度。
    pub weekly_remaining_percent: Option<f32>,
    /// 5h 紫色底浪；不存在或未知时均不绘制第二层数值/波面。
    pub five_hour_remaining_percent: Option<f32>,
    pub activity: ActivityState,
    /// 连续流动时间（秒）。仅限 24 小时范围以保留 `f32` 精度；渲染层用它产生
    /// 平滑的非同步扰动，不承担业务时间。
    pub phase: f32,
    /// 状态切换前的前景色来源。仅视觉过渡使用，不改变真实活动状态或额度数据。
    pub previous_activity: ActivityState,
    /// 新状态色从左向右替换旧色的进度，0 表示刚进入，1 表示已完全替换。
    pub state_transition_progress: f32,
}

/// 构建全部绘制模型；所有输入和输出均是 DPI 无关的 DIP。
#[must_use]
pub fn render_model(rings: QuotaRingsInput, lamp_bounds: DipRect, lamp: ActivityLampInput) -> RenderModel {
    // 先在整数毫秒域限制到一天，避免长期运行后直接转 f32 造成精度丢失。这里
    // 传给渲染器的是连续秒数而非 0..1 循环相位，杜绝每数秒一次的跳变。
    let fluid = fluid_quota_model(rings, lamp.state, (lamp.now_ms % 86_400_000) as f32);
    let (lamp, animation) = activity_lamp_model(lamp_bounds, lamp);
    let rings = quota_rings_model(rings);
    RenderModel {
        summary_left: rings.weekly.center.x + rings.weekly.radius + 7.0,
        taskbar_background_opacity: 0.70,
        rings,
        lamp,
        fluid,
        animation,
        show_quota: true,
        show_lamp: true,
    }
}

/// 从额度和活动语义生成同一水域的双层海浪模型。
#[must_use]
pub fn fluid_quota_model(input: QuotaRingsInput, activity: ActivityState, now_ms: f32) -> FluidQuotaModel {
    let weekly_remaining_percent = match input.weekly {
        ProgressValue::Known { remaining_percent } => Some(remaining_percent.clamp(0.0, 100.0)),
        ProgressValue::Unavailable => None,
    };
    let five_hour_remaining_percent = match input.five_hour {
        FiveHourProgress::Present(ProgressValue::Known { remaining_percent }) => {
            Some(remaining_percent.clamp(0.0, 100.0))
        }
        FiveHourProgress::Present(ProgressValue::Unavailable)
        | FiveHourProgress::Absent
        | FiveHourProgress::Unknown => None,
    };
    FluidQuotaModel {
        bounds: input.bounds,
        weekly_remaining_percent,
        five_hour_remaining_percent,
        activity,
        phase: now_ms.max(0.0) / 1_000.0,
        previous_activity: activity,
        state_transition_progress: 1.0,
    }
}

/// 保留旧状态灯几何兼容层，并为主流场提供恒定帧调度。
#[must_use]
pub fn activity_lamp_model(bounds: DipRect, input: ActivityLampInput) -> (ActivityLampModel, AnimationSchedule) {
    // 与原生宿主的 WM_TIMER 保持一致：正常模式持续约 60 FPS。
    const FRAME_MS: u64 = 16;
    let style = input.state.lamp_style();
    let elapsed_ms = input.now_ms.saturating_sub(input.entered_at_ms);
    let breathing = true;
    let pulse = breathe_curve(elapsed_ms);
    let center = bounds.center();
    let base_radius = (bounds.width().min(bounds.height()) / 2.0).max(0.0);
    let core = Circle { center, radius: (base_radius * 0.32).max(1.0) };
    let glow = style.glow.then(|| Glow {
        circle: Circle { center, radius: (base_radius * (0.74 + 0.10 * pulse)).max(core.radius) },
        opacity: 0.20 + 0.18 * pulse,
    });
    let dirty_bounds = bounds.inset(-2.0);
    (
        ActivityLampModel { color: style.color, core, glow, dirty_bounds },
        AnimationSchedule { next_frame_at_ms: breathing.then(|| input.now_ms.saturating_add(FRAME_MS)) },
    )
}

/// 平滑的呼吸曲线，输出在 0..=1。周期为 1.2 秒，使 60 FPS 下无突变。
#[must_use]
pub fn breathe_curve(elapsed_ms: u64) -> f32 {
    let phase = (elapsed_ms % 1_200) as f32 / 1_200.0;
    (1.0 - (TAU * phase).cos()) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: DipRect = DipRect { left: 0.0, top: 0.0, right: 30.0, bottom: 30.0 };

    #[test]
    fn nested_rings_share_center_and_inner_ring_is_smaller() {
        let model = quota_rings_model(QuotaRingsInput {
            bounds: BOUNDS,
            weekly: ProgressValue::Known { remaining_percent: 80.0 },
            five_hour: FiveHourProgress::Present(ProgressValue::Known { remaining_percent: 40.0 }),
        });
        let inner = model.five_hour.expect("5h 已存在时必须有内环");
        assert_eq!(model.weekly.center, inner.center);
        assert!(model.weekly.radius > inner.radius);
        assert_eq!(model.weekly.start_angle, -FRAC_PI_2);
    }

    #[test]
    fn absent_five_hour_hides_inner_ring_and_thickens_weekly() {
        let nested = quota_rings_model(QuotaRingsInput {
            bounds: BOUNDS,
            weekly: ProgressValue::Known { remaining_percent: 50.0 },
            five_hour: FiveHourProgress::Present(ProgressValue::Known { remaining_percent: 50.0 }),
        });
        let single = quota_rings_model(QuotaRingsInput {
            bounds: BOUNDS,
            weekly: ProgressValue::Known { remaining_percent: 50.0 },
            five_hour: FiveHourProgress::Absent,
        });
        assert!(single.five_hour.is_none());
        assert!(single.weekly.stroke_width > nested.weekly.stroke_width);
        assert_eq!(single.weekly.center, BOUNDS.center());
    }

    #[test]
    fn remaining_percent_is_clamped_to_valid_arc() {
        assert_eq!(remaining_percent_to_sweep(-10.0), 0.0);
        assert_eq!(remaining_percent_to_sweep(120.0), TAU);
        assert_eq!(remaining_percent_to_sweep(f32::NAN), 0.0);
        assert!((remaining_percent_to_sweep(25.0) - TAU / 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fluid_layers_share_one_viewport_and_long_uptime_keeps_a_valid_phase() {
        let input = QuotaRingsInput {
            bounds: BOUNDS,
            weekly: ProgressValue::Known { remaining_percent: 68.0 },
            five_hour: FiveHourProgress::Present(ProgressValue::Known { remaining_percent: 43.0 }),
        };
        let model = render_model(
            input,
            BOUNDS,
            ActivityLampInput { state: ActivityState::Executing, entered_at_ms: 0, now_ms: u64::MAX - 123 },
        );
        assert_eq!(model.fluid.bounds, BOUNDS);
        assert_eq!(model.fluid.weekly_remaining_percent, Some(68.0));
        assert_eq!(model.fluid.five_hour_remaining_percent, Some(43.0));
        assert!((0.0..86_400.0).contains(&model.fluid.phase));
    }

    #[test]
    fn fluid_time_does_not_wrap_at_the_former_short_animation_cycle() {
        let input = QuotaRingsInput {
            bounds: BOUNDS,
            weekly: ProgressValue::Known { remaining_percent: 68.0 },
            five_hour: FiveHourProgress::Absent,
        };
        let before = fluid_quota_model(input, ActivityState::Executing, 4_199.0);
        let after = fluid_quota_model(input, ActivityState::Executing, 4_201.0);
        // 旧实现会在这里由接近 1 直接跳回 0；连续秒数保证边缘形态不重置。
        assert!(after.phase > before.phase);
        assert!((after.phase - before.phase - 0.002).abs() < 0.0001);
    }

    #[test]
    fn all_activity_states_keep_the_fluid_clock_running_at_sixty_fps() {
        let idle = activity_lamp_model(
            BOUNDS,
            ActivityLampInput { state: ActivityState::Idle, entered_at_ms: 100, now_ms: 3_099 },
        )
        .1;
        let unknown = activity_lamp_model(
            BOUNDS,
            ActivityLampInput { state: ActivityState::Unknown, entered_at_ms: 100, now_ms: 30_100 },
        )
        .1;
        assert_eq!(idle.next_frame_at_ms, Some(3_115));
        assert_eq!(unknown.next_frame_at_ms, Some(30_116));
    }

    #[test]
    fn running_states_continue_at_sixty_fps() {
        let (lamp, animation) = activity_lamp_model(
            BOUNDS,
            ActivityLampInput { state: ActivityState::Executing, entered_at_ms: 0, now_ms: 60_000 },
        );
        assert_eq!(animation.next_frame_at_ms, Some(60_016));
        assert_eq!(lamp.dirty_bounds, BOUNDS.inset(-2.0));
    }
}

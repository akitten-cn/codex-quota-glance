//! 独立 HWND 宿主的线程安全命令接口。
//!
//! Windows 实现运行专用 UI 线程和消息循环；其他平台保留同样的模型与命令类型，
//! 但 `spawn` 会明确返回 [`PlatformError::UnsupportedPlatform`]。

use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, SendError, Sender, TryRecvError},
};

use codex_taskbar_domain::activity::ActivityState;
use thiserror::Error;

use crate::{
    PlatformError,
    geometry::PixelRect,
    render_model::{DipRect, FiveHourProgress, ProgressValue, QuotaRingsInput},
};

/// Explorer 任务栏父窗口的短生命周期信息。
///
/// HWND 只在当前 Explorer 生命周期有效；`screen_rect` 用于把布局器产生的屏幕
/// 坐标转换为任务栏 client 坐标。Explorer 重建后必须整体替换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskbarParent {
    pub hwnd: isize,
    pub screen_rect: PixelRect,
}

/// 原生任务栏子窗口的初始物理像素位置和可见性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeHostConfig {
    pub rect: PixelRect,
    pub taskbar_parent: TaskbarParent,
    pub initially_visible: bool,
}

impl Default for NativeHostConfig {
    fn default() -> Self {
        Self {
            rect: PixelRect { left: 0, top: 0, right: 160, bottom: 32 },
            taskbar_parent: TaskbarParent {
                hwnd: 0,
                screen_rect: PixelRect { left: 0, top: 0, right: 1920, bottom: 48 },
            },
            initially_visible: false,
        }
    }
}

/// 由宿主绘制的语义化快照。窗口线程会在活动状态变化时重置 3 秒过渡动画。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeHostModel {
    pub quota: QuotaRingsInput,
    pub lamp_bounds: DipRect,
    pub activity: ActivityState,
    pub show_quota: bool,
    pub show_lamp: bool,
    pub summary_left: f32,
    /// 用户要求减少动画时，保留静态状态灯与光晕，但禁止调度呼吸动画帧。
    pub reduce_motion: bool,
    /// 任务栏未消耗区域的深色玻璃不透明度，范围 0.20..=1.00。
    pub taskbar_background_opacity: f32,
}

impl Default for NativeHostModel {
    fn default() -> Self {
        Self {
            quota: QuotaRingsInput {
                bounds: DipRect { left: 4.0, top: 4.0, right: 28.0, bottom: 28.0 },
                weekly: ProgressValue::Unavailable,
                five_hour: FiveHourProgress::Unknown,
            },
            lamp_bounds: DipRect { left: 36.0, top: 4.0, right: 60.0, bottom: 28.0 },
            activity: ActivityState::Unknown,
            show_quota: true,
            show_lamp: true,
            summary_left: 67.0,
            reduce_motion: false,
            taskbar_background_opacity: 0.70,
        }
    }
}

/// 详情卡片中的一个“标签/值”数据行。平台层只负责排版，不理解业务字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDetailRow {
    pub label: String,
    pub value: String,
    pub kind: NativeDetailRowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativeDetailRowKind {
    #[default]
    Value,
    Section,
}

impl NativeDetailRow {
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into(), kind: NativeDetailRowKind::Value }
    }

    #[must_use]
    pub fn section(label: impl Into<String>) -> Self {
        Self { label: label.into(), value: String::new(), kind: NativeDetailRowKind::Section }
    }
}

/// 详情卡片顶部的关键指标卡。平台层只根据 tone 选择低饱和背景色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativeMetricTone {
    Positive,
    Warning,
    Critical,
    #[default]
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMetricCard {
    pub label: String,
    pub value: String,
    pub detail: String,
    pub tone: NativeMetricTone,
    /// `0..=100` 的指标进度；存在时在卡片底部绘制轻量水平条。
    pub progress_percent: Option<u8>,
}

impl NativeMetricCard {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        value: impl Into<String>,
        detail: impl Into<String>,
        tone: NativeMetricTone,
    ) -> Self {
        Self { label: label.into(), value: value.into(), detail: detail.into(), tone, progress_percent: None }
    }

    /// 为额度指标附加一个经过裁剪的剩余百分比进度条。
    #[must_use]
    pub fn with_progress(mut self, progress_percent: u8) -> Self {
        self.progress_percent = Some(progress_percent.min(100));
        self
    }
}

/// 详情卡片中的一段横向占比图。`value` 是原始权重，渲染层只计算相对宽度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeChartSegment {
    pub label: String,
    pub value: u64,
    pub tone: NativeChartTone,
}

impl NativeChartSegment {
    #[must_use]
    pub fn new(label: impl Into<String>, value: u64, tone: NativeChartTone) -> Self {
        Self { label: label.into(), value, tone }
    }
}

/// Token 构成图使用的固定色阶，避免业务层传递任意颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeChartTone {
    Input,
    Cached,
    Output,
}

/// 详情卡中的时间序列点。平台层只负责按给定顺序绘制，不解释日期语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeTrendPoint {
    /// 横轴短标签，例如 `8/19`。
    pub label: String,
    pub value: u64,
}

/// 详情 WebView 的一个可切换真实趋势。空点集表示该口径当前没有可靠数据，
/// 页面必须显示说明而不是注入演示曲线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeTrendSeries {
    pub id: String,
    pub title: String,
    pub unit: String,
    pub empty_message: String,
    pub points: Vec<NativeTrendPoint>,
}

impl NativeTrendSeries {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        unit: impl Into<String>,
        empty_message: impl Into<String>,
        points: Vec<NativeTrendPoint>,
    ) -> Self {
        Self { id: id.into(), title: title.into(), unit: unit.into(), empty_message: empty_message.into(), points }
    }
}

/// Token 费用的展示快照。金额使用微美元整数，避免把浮点舍入误报为账单。
/// `Official` 表示 App Server 已提供官方估算；`Equivalent` 表示使用明确模型
/// 和官方 API 单价计算的等价值。两者都不代表 ChatGPT/Codex 订阅实际扣费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeApiCostSource {
    Official,
    Equivalent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeApiCostEstimate {
    pub amount_micro_usd: u64,
    pub model: Option<String>,
    pub source: NativeApiCostSource,
}

impl NativeApiCostEstimate {
    #[must_use]
    pub fn display_value(&self) -> String {
        let dollars = self.amount_micro_usd / 1_000_000;
        let fraction = self.amount_micro_usd % 1_000_000;
        let source = match self.source {
            NativeApiCostSource::Official => "官方估算 · 非账单",
            NativeApiCostSource::Equivalent => "API 等价估算",
        };
        match self.model.as_deref().filter(|model| !model.is_empty()) {
            Some(model) => format!("US${dollars}.{fraction:06}（{source} · {model}）"),
            None => format!("US${dollars}.{fraction:06}（{source}）"),
        }
    }

    /// Token 快览使用短金额，避免把来源说明挤进六列窄布局；详情卡仍显示
    /// [`Self::display_value`] 的完整“官方估算 · 非账单”标识。
    #[must_use]
    pub fn compact_display_value(&self) -> String {
        let rounded_ten_thousandths = (u128::from(self.amount_micro_usd) + 50) / 100;
        let dollars = rounded_ten_thousandths / 10_000;
        let fraction = rounded_ten_thousandths % 10_000;
        format!("${dollars}.{fraction:04}")
    }
}

impl NativeTrendPoint {
    #[must_use]
    pub fn new(label: impl Into<String>, value: u64) -> Self {
        Self { label: label.into(), value }
    }
}

/// 原生详情卡片内容；由运行时把稳定聚合状态格式化为语义数据并推送给宿主。
///
/// `body` 仅作为兼容/诊断文本保留，任务栏上方详情卡片优先使用其余结构化字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeHostDetails {
    pub title: String,
    pub badge: String,
    pub status: String,
    pub updated: String,
    pub hero_label: String,
    pub hero_value: String,
    pub hero_hint: String,
    /// 非空时使用“Hero + 指标卡 + 双栏”的仪表盘布局。
    pub metric_cards: Vec<NativeMetricCard>,
    /// 官方账户模式使用紧凑左栏，把主要宽度留给右侧 Token 用量仪表盘。
    pub compact_primary_column: bool,
    /// 始终可见的数据源健康摘要，例如账户/额度/账户活动各自的新鲜度。
    pub health_rows: Vec<NativeDetailRow>,
    pub primary_heading: String,
    pub secondary_heading: String,
    pub primary_rows: Vec<NativeDetailRow>,
    pub secondary_rows: Vec<NativeDetailRow>,
    /// 任务栏上方窄滑块使用的高频 Token 指标，保持 3–5 项。
    pub quick_rows: Vec<NativeDetailRow>,
    /// 可选的 Token 构成图；缓存输入属于输入子集，调用方应先换算为互斥权重。
    pub chart_segments: Vec<NativeChartSegment>,
    /// 构成图的数据口径，例如“本机线程”或“今日 API”；为空时不绘制标题。
    pub chart_title: String,
    /// 可选的真实历史趋势；少于两个有效点时渲染层自动隐藏。
    pub trend_points: Vec<NativeTrendPoint>,
    pub trend_title: String,
    pub trend_series: Vec<NativeTrendSeries>,
    /// 可由 runtime 在收到 App Server `estimatedUsageUsdMicros` 后补入；为空时
    /// 详情层必须显示“无法估算”，不能从订阅额度猜测实际账单。
    pub api_cost_estimate: Option<NativeApiCostEstimate>,
    pub footer: String,
    pub body: String,
    pub summary_lines: [Option<String>; 2],
}

/// 原生托盘通知的视觉语义；平台层负责映射为 Windows 通知样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeNotificationKind {
    Info,
    Error,
}

/// 应用运行时提交给 Windows UI 线程的脱敏用户通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNotification {
    pub title: String,
    pub message: String,
    pub kind: NativeNotificationKind,
}

impl NativeNotification {
    #[must_use]
    pub fn new(title: impl Into<String>, message: impl Into<String>, kind: NativeNotificationKind) -> Self {
        Self { title: title.into(), message: message.into(), kind }
    }
}

impl Default for NativeHostDetails {
    fn default() -> Self {
        Self {
            title: "Codex Taskbar".to_owned(),
            badge: "官方账户".to_owned(),
            status: "等待官方数据".to_owned(),
            updated: "等待首次刷新".to_owned(),
            hero_label: "今日消耗".to_owned(),
            hero_value: "--".to_owned(),
            hero_hint: "正在连接官方账户".to_owned(),
            metric_cards: Vec::new(),
            compact_primary_column: false,
            health_rows: Vec::new(),
            primary_heading: "账户与额度".to_owned(),
            secondary_heading: "今日统计".to_owned(),
            primary_rows: Vec::new(),
            secondary_rows: Vec::new(),
            quick_rows: Vec::new(),
            chart_segments: Vec::new(),
            chart_title: String::new(),
            trend_points: Vec::new(),
            trend_title: String::new(),
            trend_series: Vec::new(),
            api_cost_estimate: None,
            footer: "单击任务栏组件查看详情".to_owned(),
            body: "正在读取官方额度与本机统计。".to_owned(),
            summary_lines: [Some("今日 --".to_owned()), Some("缓存 --".to_owned())],
        }
    }
}

impl NativeHostDetails {
    /// 注入 runtime 已确认的官方费用快照，并同步更新详情卡与 Token 快览中
    /// 的占位行。这个入口不计算价格，因而不会猜模型或把订阅额度当成账单。
    pub fn apply_api_cost_estimate(&mut self, estimate: NativeApiCostEstimate) {
        let value = estimate.display_value();
        let compact_value = estimate.compact_display_value();
        self.api_cost_estimate = Some(estimate);
        for row in &mut self.secondary_rows {
            if row.label == "API 等价费用" {
                row.value = value.clone();
            }
        }
        for row in &mut self.quick_rows {
            if row.label == "API 费用" {
                row.value = compact_value.clone();
            }
        }
    }
}

/// 投递给 UI 线程的命令。它不携带 HWND 指针，因而可安全跨线程传递。
#[derive(Debug, Clone, PartialEq)]
pub enum NativeHostCommand {
    Show,
    Hide,
    Relocate(PixelRect),
    /// Explorer 或目标显示器变化后，挂接到新的任务栏 HWND 并应用屏幕坐标矩形。
    AttachToTaskbar {
        parent: TaskbarParent,
        rect: PixelRect,
    },
    UpdateModel(NativeHostModel),
    /// 由应用层 [`TaskbarSnapshot`](codex_taskbar_application::ui_snapshot::TaskbarSnapshot)
    /// 序列化后的脱敏 JSON。平台层不读取账户、SQLite 或凭据，只在 UI 线程把它
    /// 原样转交给本地 WebView2 页面。
    ///
    /// 调用方必须使用应用层的展示快照，不能把原始 App Server/SQLite JSON 送入
    /// 页面；这条边界可阻止线程标识、提示词和凭据进入浏览器渲染进程。
    UpdateWebTaskbarSnapshot(Box<str>),
    /// 本次消耗浮窗的脱敏 JSON；仅在已打开的 WebView2 Token 快览中投递。
    UpdateWebTokenStripSnapshot(Box<str>),
    UpdateDetails(Box<NativeHostDetails>),
    /// 仅供视觉验收/显式用户交互使用；生产数据更新不会自动弹窗。
    ShowDetails,
    /// 显示或刷新 Token 快览。runtime 可在新 turn/token 增量时重复调用；
    /// 原生层会重置约 4 秒自动隐藏倒计时，不重建窗口也不抢前台焦点。
    ShowTokenStrip,
    /// Explorer 重启后重新向通知区域注册托盘图标。
    RestoreTrayIcon,
    /// 显示一条不包含凭据或完整路径的托盘通知。
    ShowNotification(Box<NativeNotification>),
    RequestExit,
}

/// 由 Windows UI 线程发布给应用运行时的用户操作。
///
/// 平台层只识别菜单意图，不直接读取配置文件或启动外部进程，以保持宿主与
/// 应用装配逻辑解耦。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeHostEvent {
    /// 用户从托盘图标请求打开详情卡片。
    ShowDetailsRequested,
    /// 用户从详情卡请求立即刷新官方数据；平台层不直接访问网络或本地数据源。
    RefreshRequested,
    /// 用户请求打开首选项窗口；当前会提示用户设置页正在重建。
    OpenSettingsRequested,
    /// 用户请求从磁盘重新读取 `settings.json`。
    ReloadSettingsRequested,
    /// 用户请求使用系统文本编辑器打开 `settings.json`。
    EditSettingsRequested,
    /// 用户请求在资源管理器中打开配置目录。
    OpenConfigDirectoryRequested,
    /// 用户请求在资源管理器中打开结构化日志目录。
    OpenLogDirectoryRequested,
    /// 用户请求正常退出应用。
    ExitRequested,
}

/// 命令无法投递或目标宿主已停止。
#[derive(Debug, Error)]
pub enum NativeHostCommandError {
    #[error("原生宿主已停止")]
    Stopped,
}

/// 可跨线程克隆的宿主命令端。
#[derive(Clone)]
pub struct NativeHostHandle {
    sender: Sender<NativeHostCommand>,
    events: Arc<Mutex<Receiver<NativeHostEvent>>>,
    #[cfg(all(windows, feature = "direct2d"))]
    wake: std::sync::Arc<std::sync::atomic::AtomicIsize>,
}

impl NativeHostHandle {
    /// 投递一个完整命令；Windows 上会立即唤醒 UI 消息循环。
    pub fn send(&self, command: NativeHostCommand) -> Result<(), NativeHostCommandError> {
        self.sender.send(command).map_err(map_send_error)?;
        #[cfg(all(windows, feature = "direct2d"))]
        native::wake_host(&self.wake)?;
        Ok(())
    }

    pub fn show(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::Show)
    }

    pub fn hide(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::Hide)
    }

    pub fn relocate(&self, rect: PixelRect) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::Relocate(rect))
    }

    pub fn attach_to_taskbar(&self, parent: TaskbarParent, rect: PixelRect) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::AttachToTaskbar { parent, rect })
    }

    pub fn update_model(&self, model: NativeHostModel) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::UpdateModel(model))
    }

    /// 更新 WebGL 任务栏胶囊的只读展示快照。
    ///
    /// 字符串的所有权在命令入队后转移给 UI 线程；没有 WebView2 时该命令会被
    /// 安全忽略，原生降级渲染仍可继续工作。
    pub fn update_web_taskbar_snapshot(&self, snapshot_json: String) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::UpdateWebTaskbarSnapshot(snapshot_json.into_boxed_str()))
    }

    /// 更新已打开或即将打开的 Token 快览。调用者只能传入应用层的
    /// `ConsumptionPopupSnapshot` JSON，禁止原始协议、线程 ID 和 Prompt。
    pub fn update_web_token_strip_snapshot(&self, snapshot_json: String) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::UpdateWebTokenStripSnapshot(snapshot_json.into_boxed_str()))
    }

    pub fn update_details(&self, details: NativeHostDetails) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::UpdateDetails(Box::new(details)))
    }

    pub fn show_details(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::ShowDetails)
    }

    pub fn show_token_strip(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::ShowTokenStrip)
    }

    /// `ShowTokenStrip` 的语义化别名，供 token 增量路径表达“刷新快览”意图。
    /// 保留 `show_token_strip` 兼容现有视觉预览和调用方。
    pub fn refresh_token_strip(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::ShowTokenStrip)
    }

    /// Explorer 重建通知区域后重新注册托盘图标。
    pub fn restore_tray_icon(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::RestoreTrayIcon)
    }

    /// 在通知区域显示一条短消息；正文必须先由应用层脱敏。
    pub fn show_notification(&self, notification: NativeNotification) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::ShowNotification(Box::new(notification)))
    }

    /// 非阻塞读取一条用户操作；没有操作时返回 `Ok(None)`。
    pub fn try_recv_event(&self) -> Result<Option<NativeHostEvent>, NativeHostCommandError> {
        let receiver = self.events.lock().map_err(|_| NativeHostCommandError::Stopped)?;
        match receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(NativeHostCommandError::Stopped),
        }
    }

    pub fn request_exit(&self) -> Result<(), NativeHostCommandError> {
        self.send(NativeHostCommand::RequestExit)
    }
}

fn map_send_error(_: SendError<NativeHostCommand>) -> NativeHostCommandError {
    NativeHostCommandError::Stopped
}

/// 创建透明的 Explorer 任务栏子窗口和它的专用消息循环。
///
/// 返回后窗口已创建，因此返回的 [`NativeHostHandle`] 可立即从任意线程调用。窗口
/// 调用方应先用 `geometry::layout_probe` 计算安全的屏幕位置，并提供同一快照中的
/// 任务栏 HWND；宿主内部负责转换为 parent-client 坐标。
pub fn spawn_native_host(config: NativeHostConfig, model: NativeHostModel) -> Result<NativeHostHandle, PlatformError> {
    #[cfg(all(windows, feature = "direct2d"))]
    {
        native::spawn(config, model)
    }
    #[cfg(not(all(windows, feature = "direct2d")))]
    {
        let _ = (config, model);
        Err(PlatformError::UnsupportedPlatform)
    }
}

/// 宿主内的纯状态机。测试可验证状态切换是否会启动或停止动画，无需创建 HWND。
#[cfg(any(all(windows, feature = "direct2d"), test))]
#[derive(Debug, Clone)]
struct HostRuntime {
    model: NativeHostModel,
    activity_entered_at_ms: u64,
    previous_activity: ActivityState,
}

#[cfg(any(all(windows, feature = "direct2d"), test))]
impl HostRuntime {
    fn new(model: NativeHostModel, now_ms: u64) -> Self {
        Self { previous_activity: model.activity, model, activity_entered_at_ms: now_ms }
    }

    fn update(&mut self, model: NativeHostModel, now_ms: u64) {
        if self.model.activity != model.activity {
            self.previous_activity = self.model.activity;
            self.activity_entered_at_ms = now_ms;
        }
        self.model = model;
    }

    fn frame(&self, now_ms: u64) -> crate::render_model::RenderModel {
        let mut model = crate::render_model::render_model(
            self.model.quota,
            self.model.lamp_bounds,
            crate::render_model::ActivityLampInput {
                state: self.model.activity,
                entered_at_ms: self.activity_entered_at_ms,
                now_ms,
            },
        );
        // 颜色不瞬切：新的状态色从左侧以一股翻滚流体推进，约 900ms 后覆盖
        // 已消耗区域。额度宽度仍只由官方剩余百分比决定。
        model.fluid.previous_activity = self.previous_activity;
        model.fluid.state_transition_progress =
            ((now_ms.saturating_sub(self.activity_entered_at_ms)) as f32 / 900.0).clamp(0.0, 1.0);
        model.show_quota = self.model.show_quota;
        model.show_lamp = self.model.show_lamp;
        model.summary_left = self.model.summary_left;
        model.taskbar_background_opacity = self.model.taskbar_background_opacity.clamp(0.20, 1.0);
        // V2 的动画归属于海浪额度组件，不再依赖已经移除的独立状态灯。
        if !model.show_quota || self.model.reduce_motion {
            model.animation.next_frame_at_ms = None;
        }
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_cost_snapshot_is_rendered_as_non_bill_estimate() {
        let estimate =
            NativeApiCostEstimate { amount_micro_usd: 1_250, model: None, source: NativeApiCostSource::Official };
        assert_eq!(estimate.display_value(), "US$0.001250（官方估算 · 非账单）");

        let mut details = NativeHostDetails {
            secondary_rows: vec![NativeDetailRow::new("API 等价费用", "--")],
            quick_rows: vec![NativeDetailRow::new("API 费用", "--")],
            ..NativeHostDetails::default()
        };
        details.apply_api_cost_estimate(estimate);
        assert!(details.secondary_rows[0].value.contains("官方估算"));
        assert_eq!(details.quick_rows[0].value, "$0.0013");
    }

    #[test]
    fn normal_mode_keeps_the_fluid_clock_running() {
        let runtime = HostRuntime::new(NativeHostModel::default(), 10);
        assert_eq!(runtime.frame(10).animation.next_frame_at_ms, Some(26));
    }

    #[test]
    fn activity_changes_do_not_interrupt_the_continuous_fluid_clock() {
        let mut runtime =
            HostRuntime::new(NativeHostModel { activity: ActivityState::Idle, ..NativeHostModel::default() }, 10);
        runtime.update(NativeHostModel { activity: ActivityState::Idle, ..NativeHostModel::default() }, 2_000);
        assert_eq!(runtime.frame(3_005).animation.next_frame_at_ms, Some(3_021));

        runtime.update(NativeHostModel { activity: ActivityState::Completed, ..NativeHostModel::default() }, 4_000);
        assert_eq!(runtime.frame(6_999).animation.next_frame_at_ms, Some(7_015));
        assert_eq!(runtime.frame(7_000).animation.next_frame_at_ms, Some(7_016));
    }

    #[test]
    fn reduce_motion_keeps_lamp_visible_but_never_schedules_animation() {
        let runtime = HostRuntime::new(
            NativeHostModel { activity: ActivityState::Executing, reduce_motion: true, ..NativeHostModel::default() },
            10,
        );
        let frame = runtime.frame(1_000);
        assert!(frame.show_lamp);
        assert!(frame.lamp.glow.is_some());
        assert_eq!(frame.animation.next_frame_at_ms, None);
    }
}

#[cfg(all(windows, feature = "direct2d"))]
mod native;

//! 基于 `windows-rs` 的 Explorer 任务栏发现与独立浮窗探针。

use std::mem::size_of;

use windows::{
    Win32::{
        Foundation::{FILETIME, HWND, RECT, SYSTEMTIME},
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow},
        System::{
            SystemInformation::GetLocalTime,
            Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
        },
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetDpiForWindow,
                SetProcessDpiAwarenessContext,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, FindWindowExW, FindWindowW, GetWindowRect, HWND_TOPMOST, SW_SHOWNA,
                SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos, ShowWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::w,
};

use crate::{
    PlatformError, ProbeConfig,
    geometry::{PixelRect, TaskbarGeometry, layout_probe},
};
use codex_taskbar_domain::layout::TaskbarAnchor;

/// 将 Unix 秒转换为当前 Windows 时区的精确本地时间。
///
/// 额度重置时间由服务端以 UTC Unix 秒给出。这里使用 Windows 自身时区规则
/// （包含夏令时），不引入额外日期库，也不会把 UTC 误显示成本地时间。
#[must_use]
pub fn format_local_unix_time(timestamp_unix: i64) -> Option<String> {
    if timestamp_unix <= 0 {
        return None;
    }
    let filetime_ticks = u64::try_from(timestamp_unix).ok()?.checked_add(11_644_473_600)?.checked_mul(10_000_000)?;
    let filetime = FILETIME { dwLowDateTime: filetime_ticks as u32, dwHighDateTime: (filetime_ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // 两个 API 仅写入调用方提供的结构体；错误时回退到相对倒计时，避免显示伪造日期。
    unsafe {
        FileTimeToSystemTime(&filetime, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute))
}

/// 返回 Windows 当前本地日期和小时，供本机日用量账本分桶。
///
/// 使用系统时区规则而非 Unix UTC 天数，避免中国午夜附近的累计被错误记到前一天。
#[must_use]
pub fn local_usage_clock() -> (i32, u8) {
    let local = unsafe { GetLocalTime() };
    let day_key = i32::from(local.wYear) * 10_000 + i32::from(local.wMonth) * 100 + i32::from(local.wDay);
    (day_key, u8::try_from(local.wHour).unwrap_or(0).min(23))
}

/// Explorer 任务栏窗口的已知顶层类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarWindowClass {
    /// 主显示器任务栏。
    Primary,
    /// 次显示器任务栏。
    Secondary,
}

/// 一个只读发现到的 Explorer 任务栏快照。
///
/// `hwnd` 仅用于同一 Explorer 生命周期内的短暂比较；Explorer 重启后必须重新发现，
/// 不能将其持久化或跨进程解引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTaskbar {
    pub hwnd: isize,
    pub class: TaskbarWindowClass,
    pub monitor_device: String,
    pub geometry: TaskbarGeometry,
}

/// 按配置查找任务栏并计算探针矩形的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePlacement {
    pub taskbar: DiscoveredTaskbar,
    pub rect: PixelRect,
}

/// 枚举主、次显示器的 `Shell_TrayWnd` 与 `Shell_SecondaryTrayWnd`。
///
/// 此函数只有 `FindWindow*`、监视器和窗口矩形读取操作，不改变 Explorer 窗口层级或样式。
pub fn discover_taskbars() -> Result<Vec<DiscoveredTaskbar>, PlatformError> {
    let mut windows = Vec::new();
    // SAFETY: 类名为静态的 NUL 结尾 UTF-16 字面量；返回 HWND 只在本函数内立即读取。
    // FindWindow* 在“未找到”时返回 NULL，但 Win32 文档明确说明它不会设置 LastError。
    // windows-rs 会把 NULL 转成 Error；若直接传播，错误可能来自线程上一次调用（常见为
    // E_ACCESSDENIED），因此这里把 NULL 规范化为空 HWND，再由 TaskbarNotFound 表达语义。
    let primary = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default();
    if !primary.0.is_null() {
        windows.push((primary, TaskbarWindowClass::Primary));
    }

    let mut after = None;
    loop {
        // SAFETY: 仅枚举桌面顶层窗口，不向 Explorer 发送消息或写入其状态。
        let next = unsafe { FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), None) }.unwrap_or_default();
        if next.0.is_null() {
            break;
        }
        windows.push((next, TaskbarWindowClass::Secondary));
        after = Some(next);
    }

    windows.into_iter().map(|(hwnd, class)| snapshot_taskbar(hwnd, class)).collect()
}

/// 使用显示器设备名（如 `\\\\.\\DISPLAY2`）选择任务栏；未指定时优先第一个副屏任务栏。
///
/// 用户明确选择的设备永远不回退到其它屏幕；只有“自动”模式会在没有副屏时安全回退主屏。
pub fn discover_probe_placement(config: &ProbeConfig) -> Result<ProbePlacement, PlatformError> {
    let taskbars = discover_taskbars()?;
    let selected = select_taskbar(&taskbars, config.target_monitor_device.as_deref(), config.prefer_secondary_monitor)
        .ok_or(PlatformError::TaskbarNotFound)?
        .clone();
    let rect = layout_probe_with_safe_left_fallback(&selected.geometry, config)?;
    Ok(ProbePlacement { taskbar: selected, rect })
}

/// 右侧通知区域只在部分 Windows 多屏任务栏组合中暴露。缺少边界时若仍猜测
/// 屏幕右缘，会直接覆盖系统/第三方组件；因此同一任务栏安全退到左侧，而不是
/// 让整个常驻程序无法启动。显式右侧偏好保持在配置中，后续 Explorer 暴露安全
/// 边界后下一次布局刷新会自动恢复靠右。
fn layout_probe_with_safe_left_fallback(
    geometry: &TaskbarGeometry,
    config: &ProbeConfig,
) -> Result<PixelRect, PlatformError> {
    match layout_probe(geometry, config) {
        Err(PlatformError::MissingRightSafetyBoundary) if config.anchor == TaskbarAnchor::Right => {
            tracing::warn!(event = "taskbar_right_boundary_missing", "右侧通知区边界不可用，安全回退到任务栏左侧");
            let mut fallback = config.clone();
            fallback.anchor = TaskbarAnchor::Left;
            layout_probe(geometry, &fallback)
        }
        result => result,
    }
}

/// 选择已发现任务栏的纯逻辑，供调用方在 Explorer 重启后以新快照重新匹配。
#[must_use]
pub fn select_taskbar<'a>(
    taskbars: &'a [DiscoveredTaskbar],
    target_monitor_device: Option<&str>,
    prefer_secondary_monitor: bool,
) -> Option<&'a DiscoveredTaskbar> {
    match target_monitor_device {
        Some(device) => taskbars.iter().find(|taskbar| taskbar.monitor_device.eq_ignore_ascii_case(device)),
        None if prefer_secondary_monitor => taskbars
            .iter()
            .find(|taskbar| taskbar.class == TaskbarWindowClass::Secondary)
            .or_else(|| taskbars.iter().find(|taskbar| taskbar.class == TaskbarWindowClass::Primary))
            .or_else(|| taskbars.first()),
        None => taskbars
            .iter()
            .find(|taskbar| taskbar.class == TaskbarWindowClass::Primary)
            .or_else(|| taskbars.iter().find(|taskbar| taskbar.class == TaskbarWindowClass::Secondary))
            .or_else(|| taskbars.first()),
    }
}

/// 请求将当前进程设为 Per-Monitor V2 DPI aware。
///
/// 应在创建任何 UI 窗口之前、且由进程入口显式调用一次；失败通常说明宿主已选择
/// 其他 DPI 上下文，此时仍会用任务栏 HWND 的实际 DPI 进行只读布局。
pub fn enable_per_monitor_dpi_awareness() -> Result<(), PlatformError> {
    // SAFETY: Win32 要求在 UI 初始化前调用；函数不接收或保留 Rust 指针。
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.map_err(windows_error)
}

/// 独立探针窗口。它从不调用 `SetParent`，因而不改变 Explorer 的窗口树。
pub struct FloatingProbeWindow {
    hwnd: HWND,
}

impl FloatingProbeWindow {
    /// 创建并显示无激活的顶层浮窗。
    ///
    /// 仅接受已经通过 `layout_probe` 验证的安全矩形。虽然窗口置顶以显示在任务栏上方，
    /// 它没有 Explorer 父窗口，也不会写入或重排用户的任务栏组件。
    pub fn create(placement: &ProbePlacement) -> Result<Self, PlatformError> {
        let rect = placement.rect;
        // SAFETY: 使用系统注册的 STATIC 类，字符串均为静态 NUL 结尾；父窗口明确为 None，
        // 因此绝不会发生 SetParent 或向 Explorer 注入子窗口。
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("Codex Taskbar Probe"),
                WS_POPUP,
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                None,
                None,
                None,
                None,
            )
        }
        .map_err(windows_error)?;
        if hwnd.0.is_null() {
            return Err(PlatformError::Windows("CreateWindowExW 返回空 HWND".into()));
        }

        // SAFETY: hwnd 由本方法刚创建且仍归调用线程所有；不改变任何 Explorer HWND。
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .map_err(windows_error)?;
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
        Ok(Self { hwnd })
    }

    /// 以新的物理像素矩形移动浮窗；显示器 DPI 或 Explorer 重启后应重新发现再调用。
    pub fn move_to(&self, rect: PixelRect) -> Result<(), PlatformError> {
        // SAFETY: hwnd 只由本对象持有；调用只调整本进程自己的顶层浮窗。
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .map_err(windows_error)
        }
    }

    /// 返回短生命周期 HWND 值，仅供诊断或与 Win32 互操作使用。
    #[must_use]
    pub fn hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }
}

impl Drop for FloatingProbeWindow {
    fn drop(&mut self) {
        // SAFETY: 只销毁由本对象创建的独立窗口；失败时进程退出会清理该 HWND。
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Explorer 变更检查器。周期性调用 `refresh`；出现重启或任务栏重建时重新发现并重布局。
#[derive(Debug, Default)]
pub struct ExplorerRestartWatcher {
    last_handles: Vec<isize>,
}

/// Explorer 任务栏发现状态变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerTaskbarChange {
    Unchanged(Vec<DiscoveredTaskbar>),
    Rebuilt(Vec<DiscoveredTaskbar>),
    Missing,
}

impl ExplorerRestartWatcher {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_handles: Vec::new() }
    }

    /// 重新读取 Explorer 任务栏；不要复用旧 HWND 或旧 DPI 快照。
    pub fn refresh(&mut self) -> Result<ExplorerTaskbarChange, PlatformError> {
        let taskbars = discover_taskbars()?;
        if taskbars.is_empty() {
            self.last_handles.clear();
            return Ok(ExplorerTaskbarChange::Missing);
        }
        let handles = taskbars.iter().map(|taskbar| taskbar.hwnd).collect::<Vec<_>>();
        let changed = handles != self.last_handles;
        self.last_handles = handles;
        Ok(if changed { ExplorerTaskbarChange::Rebuilt(taskbars) } else { ExplorerTaskbarChange::Unchanged(taskbars) })
    }
}

fn snapshot_taskbar(hwnd: HWND, class: TaskbarWindowClass) -> Result<DiscoveredTaskbar, PlatformError> {
    let taskbar_rect = window_rect(hwnd)?;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: info 是适当大小且可写的 MONITORINFOEXW，转换指针符合 Win32 ABI。
    unsafe { GetMonitorInfoW(monitor, (&mut info as *mut MONITORINFOEXW).cast()) }.ok().map_err(windows_error)?;
    let monitor_rect = from_rect(info.monitorInfo.rcMonitor);
    let monitor_device = utf16z_to_string(&info.szDevice);
    // SAFETY: hwnd 是本次发现得到的有效窗口；API 只读取其 DPI。若 shell 窗口短暂消失，0 回退 96。
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { unsafe { GetDpiForSystem() }.max(96) } else { dpi };

    Ok(DiscoveredTaskbar {
        hwnd: hwnd.0 as isize,
        class,
        monitor_device,
        geometry: TaskbarGeometry {
            taskbar_rect,
            monitor_rect,
            dpi,
            right_safe_boundary_x: notification_left_boundary(hwnd),
        },
    })
}

fn notification_left_boundary(taskbar: HWND) -> Option<i32> {
    // TrayNotifyWnd 覆盖通知图标和时钟，是右锚定的首选安全边界。
    let notify = unsafe { FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), None) }.ok()?;
    if !notify.0.is_null() {
        return window_rect(notify).ok().map(|rect| rect.left);
    }
    // 有些 Explorer 版本将时钟暴露为直接子项，作为保守备选边界。
    let clock = unsafe { FindWindowExW(Some(taskbar), None, w!("TrayClockWClass"), None) }.ok()?;
    if !clock.0.is_null() {
        return window_rect(clock).ok().map(|rect| rect.left);
    }
    None
}

fn window_rect(hwnd: HWND) -> Result<PixelRect, PlatformError> {
    let mut rect = RECT::default();
    // SAFETY: RECT 是有效可写缓冲区；GetWindowRect 仅读取指定 HWND 的屏幕坐标。
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(windows_error)?;
    Ok(from_rect(rect))
}

fn from_rect(rect: RECT) -> PixelRect {
    PixelRect { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom }
}

fn utf16z_to_string(value: &[u16]) -> String {
    let end = value.iter().position(|&unit| unit == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn windows_error(error: windows::core::Error) -> PlatformError {
    PlatformError::Windows(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taskbar(hwnd: isize, class: TaskbarWindowClass, device: &str) -> DiscoveredTaskbar {
        DiscoveredTaskbar {
            hwnd,
            class,
            monitor_device: device.to_owned(),
            geometry: TaskbarGeometry {
                taskbar_rect: PixelRect { left: 0, top: 1_040, right: 1_920, bottom: 1_080 },
                monitor_rect: PixelRect { left: 0, top: 0, right: 1_920, bottom: 1_080 },
                dpi: 96,
                right_safe_boundary_x: Some(1_700),
            },
        }
    }

    #[test]
    fn explicit_monitor_device_selects_secondary_case_insensitively() {
        let taskbars = [
            taskbar(1, TaskbarWindowClass::Primary, r"\\.\DISPLAY1"),
            taskbar(2, TaskbarWindowClass::Secondary, r"\\.\DISPLAY2"),
        ];
        let selected = select_taskbar(&taskbars, Some(r"\\.\display2"), true).unwrap();
        assert_eq!(selected.hwnd, 2);
    }

    #[test]
    fn unspecified_monitor_prefers_secondary_even_when_discovery_order_differs() {
        let taskbars = [
            taskbar(2, TaskbarWindowClass::Secondary, r"\\.\DISPLAY2"),
            taskbar(1, TaskbarWindowClass::Primary, r"\\.\DISPLAY1"),
        ];
        assert_eq!(select_taskbar(&taskbars, None, true).unwrap().hwnd, 2);
    }

    #[test]
    fn unspecified_monitor_safely_falls_back_to_primary_on_a_single_display() {
        let taskbars = [taskbar(1, TaskbarWindowClass::Primary, r"\\.\DISPLAY1")];
        assert_eq!(select_taskbar(&taskbars, None, true).unwrap().hwnd, 1);
    }

    #[test]
    fn missing_explicit_monitor_never_falls_back_to_another_screen() {
        let taskbars = [taskbar(1, TaskbarWindowClass::Primary, r"\\.\DISPLAY1")];
        assert!(select_taskbar(&taskbars, Some(r"\\.\DISPLAY9"), true).is_none());
    }

    #[test]
    fn missing_right_boundary_safely_uses_left_without_changing_the_saved_preference() {
        let config = ProbeConfig {
            preferred_width_px: 320,
            edge_gap_px: 8,
            anchor: TaskbarAnchor::Right,
            ..ProbeConfig::default()
        };
        let geometry = TaskbarGeometry {
            taskbar_rect: PixelRect { left: 1_920, top: 1_040, right: 3_840, bottom: 1_080 },
            monitor_rect: PixelRect { left: 1_920, top: 0, right: 3_840, bottom: 1_080 },
            dpi: 96,
            right_safe_boundary_x: None,
        };
        assert_eq!(
            layout_probe_with_safe_left_fallback(&geometry, &config).unwrap(),
            PixelRect { left: 1_928, top: 1_040, right: 2_248, bottom: 1_080 }
        );
        assert_eq!(config.anchor, TaskbarAnchor::Right);
    }

    #[test]
    fn unix_timestamp_formats_as_a_precise_local_clock_time() {
        // 2024-01-01T00:00:00Z；具体小时取决于电脑当前时区，因此只验证
        // Windows 转换成功且输出保持固定、易读的日期时间格式。
        let formatted = format_local_unix_time(1_704_067_200).expect("Windows local time");
        assert_eq!(formatted.len(), 16);
        assert_eq!(&formatted[4..5], "-");
        assert_eq!(&formatted[10..11], " ");
        assert_eq!(&formatted[13..14], ":");
    }
}

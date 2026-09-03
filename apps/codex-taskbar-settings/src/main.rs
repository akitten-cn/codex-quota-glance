//! 嵌入 `codex-taskbar.exe` 的设置窗口。
//!
//! 设置页和任务栏、详情卡共用 WebView2 / HTML 视觉系统，但不是独立进程：它只在
//! 用户打开时由主程序同进程创建，关闭即释放。页面仅能提交白名单布局设置。

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        mpsc::Sender,
    },
};

static SETTINGS_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
static SETTINGS_WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);

/// 设置页只能向主运行时提交这些白名单动作，不能执行任意命令或传入路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    ManualRefresh,
    ShowHistory,
    ClearHistory,
    CheckUpdates,
    DownloadUpdate,
    ExportDiagnostics,
}

/// 在任务栏主程序的同一进程中打开唯一设置窗口。
pub fn launch(settings_path: PathBuf, actions: Sender<SettingsAction>) -> Result<(), String> {
    if SETTINGS_WINDOW_OPEN.swap(true, Ordering::AcqRel) {
        // 窗口可能仍在已经断开的副屏坐标中。再次点击设置必须前置现有窗口，
        // 并在它不再与任何工作区相交时搬回当前可用屏幕。
        platform::activate_existing_window(SETTINGS_WINDOW_HWND.load(Ordering::Acquire));
        return Ok(());
    }
    std::thread::Builder::new()
        .name("codex-taskbar-settings-ui".to_owned())
        .spawn(move || {
            let result = platform::run_window(settings_path, actions);
            SETTINGS_WINDOW_HWND.store(0, Ordering::Release);
            SETTINGS_WINDOW_OPEN.store(false, Ordering::Release);
            if let Err(error) = result {
                // 设置页异常不能扩大为常驻任务栏异常；不记录配置、账户或网页消息。
                tracing::warn!(event = "settings_window_closed_with_error", error = %error, "设置窗口已关闭");
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
mod platform {
    use super::SettingsAction;
    use std::{path::PathBuf, sync::mpsc::Sender};
    pub(super) fn run_window(_settings_path: PathBuf, _actions: Sender<SettingsAction>) -> Result<(), String> {
        Err("设置窗口仅支持 Windows".to_owned())
    }
    pub(super) fn activate_existing_window(_raw_hwnd: isize) {}
}

#[cfg(windows)]
mod platform {
    use std::{
        mem::size_of,
        ptr,
        str::FromStr,
        sync::{Arc, Mutex, OnceLock, atomic::Ordering, mpsc::sync_channel},
    };

    use super::{SETTINGS_WINDOW_HWND, SettingsAction};
    use codex_taskbar_settings::{
        AppConfig, LogLevel, MAX_TASKBAR_WIDTH_PX, MIN_TASKBAR_WIDTH_PX, SyncMode, TaskbarAnchor, request_reload,
    };
    use serde::Deserialize;
    use webview2_com::{
        CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
        Microsoft::Web::WebView2::Win32::*, WebMessageReceivedEventHandler,
    };
    use windows::{
        Win32::{
            Foundation::{E_INVALIDARG, HWND, LPARAM, LRESULT, RECT, WPARAM},
            Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
            System::{
                Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
                LibraryLoader::GetModuleHandleW,
            },
            UI::{
                HiDpi::GetDpiForSystem,
                Input::KeyboardAndMouse::ReleaseCapture,
                WindowsAndMessaging::{
                    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
                    GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, HTCAPTION, HWND_TOPMOST, IsWindow,
                    MONITORINFOF_PRIMARY, MSG, PostQuitMessage, RegisterClassW, SW_RESTORE, SW_SHOW, SWP_NOSIZE,
                    SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
                    ShowWindow, TranslateMessage, WM_DESTROY, WM_NCLBUTTONDOWN, WM_SIZE, WNDCLASSW, WS_EX_TOOLWINDOW,
                    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
                },
            },
        },
        core::{BOOL, Error, HSTRING, Interface, PCWSTR, PWSTR, w},
    };

    const SETTINGS_CLASS: PCWSTR = w!("CodexTaskbarSettingsWebView");
    const SETTINGS_TITLE: PCWSTR = w!("Codex Taskbar 设置");
    const SETTINGS_WEB_DOCUMENT: &str = include_str!("../../../prototypes/settings-layout-reference.html");
    const TASKBAR_WEB_DOCUMENT: &str = include_str!("../../../prototypes/fluid-front-reference.html");
    const TASKBAR_VISUAL_CONTRACT: &str = include_str!("../../../prototypes/taskbar-visual-contract.js");
    static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

    #[derive(Debug, Deserialize)]
    struct WebMessage {
        action: String,
        #[serde(default)]
        settings: Option<LayoutSettings>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        screen_x: Option<i32>,
        #[serde(default)]
        screen_y: Option<i32>,
    }

    /// 仅接受页面明确可编辑、且不会携带身份或对话数据的字段。
    #[derive(Debug, Deserialize)]
    struct LayoutSettings {
        display: String,
        dock: String,
        width: u64,
        traffic: i64,
        opacity: u64,
        log_level: String,
        #[serde(default)]
        reduce_motion: bool,
        sync_mode: String,
        history_retention_days: u64,
        log_retention_days: u64,
        adaptive_chunk_download: bool,
    }

    struct SettingsWindowContext {
        controller: ICoreWebView2Controller,
    }

    pub(super) fn run_window(
        settings_path: std::path::PathBuf,
        actions: std::sync::mpsc::Sender<SettingsAction>,
    ) -> Result<(), String> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(|error| error.to_string())? };
        let result = run_window_inner(settings_path, actions);
        unsafe { CoUninitialize() };
        result
    }

    fn run_window_inner(
        settings_path: std::path::PathBuf,
        actions: std::sync::mpsc::Sender<SettingsAction>,
    ) -> Result<(), String> {
        let config = AppConfig::load_or_create(&settings_path).map_err(|error| error.to_string())?;
        register_window_class()?;
        let prefer_secondary = config.prefer_secondary_monitor;
        let hwnd = create_window(
            settings_web_document(&config),
            settings_path,
            Arc::new(Mutex::new(config)),
            actions,
            prefer_secondary,
        )?;
        SETTINGS_WINDOW_HWND.store(hwnd.0 as isize, Ordering::Release);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            // 设置页是同进程 tool window，不进入任务栏；但它可能从置顶详情卡
            // 内打开，因此必须显式进入 topmost 层，否则窗口已经创建却被详情卡
            // 或当前前台程序遮住，用户看到的就是“点击没有反应”。
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        };
        let mut message = MSG::default();
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    fn register_window_class() -> Result<(), String> {
        if CLASS_REGISTERED.get().is_some() {
            return Ok(());
        }
        let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
        let class = WNDCLASSW {
            hInstance: instance.into(),
            lpszClassName: SETTINGS_CLASS,
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(Error::from_thread().to_string());
        }
        let _ = CLASS_REGISTERED.set(());
        Ok(())
    }

    fn create_window(
        document: String,
        settings_path: std::path::PathBuf,
        state: Arc<Mutex<AppConfig>>,
        actions: std::sync::mpsc::Sender<SettingsAction>,
        prefer_secondary: bool,
    ) -> Result<HWND, String> {
        let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
        // CSS/WebView2 用 96-DPI 逻辑像素；HWND 必须按当前 DPI 放大，才能避免
        // 125%/150% 缩放下右侧内容被裁切。
        let dpi = unsafe { GetDpiForSystem() }.max(96);
        // 额外保留左右透明安全区，确保高 DPI / WebView2 逻辑缩放组合下设置卡
        // 的右侧与右上角关闭按钮永不被客户区裁切。
        let desired_width = scale_dip(1_100, dpi);
        let desired_height = scale_dip(900, dpi);
        let margin = scale_dip(16, dpi);
        let (left, top, width, height) = preferred_window_geometry(
            desired_width,
            desired_height,
            margin,
            prefer_secondary,
        )
        .unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT, desired_width, desired_height));
        let zoom_factor = settings_zoom_factor(width, height, dpi);
        let hwnd = unsafe {
            CreateWindowExW(
                // TOOLWINDOW 保证设置仍属于任务栏软件自身，不产生独立任务栏项；
                // TOPMOST 仅用于这个显式打开的功能页，关闭窗口后自然释放。
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                SETTINGS_CLASS,
                SETTINGS_TITLE,
                WS_POPUP | WS_VISIBLE,
                left,
                top,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        let context = match create_webview(hwnd, document, settings_path, state, actions, zoom_factor) {
            Ok(context) => context,
            Err(error) => {
                let _ = unsafe { DestroyWindow(hwnd) };
                return Err(error);
            }
        };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(context)) as isize) };
        Ok(hwnd)
    }

    const fn scale_dip(value: i32, dpi: u32) -> i32 {
        ((value as i64 * dpi as i64 + 48) / 96) as i32
    }

    fn settings_zoom_factor(width: i32, height: i32, dpi: u32) -> f64 {
        const DOCUMENT_WIDTH_DIP: f64 = 980.0;
        const DOCUMENT_HEIGHT_DIP: f64 = 860.0;
        let dpi = f64::from(dpi.max(96));
        let logical_width = f64::from(width.max(1)) * 96.0 / dpi;
        let logical_height = f64::from(height.max(1)) * 96.0 / dpi;
        (logical_width / DOCUMENT_WIDTH_DIP).min(logical_height / DOCUMENT_HEIGHT_DIP).clamp(0.62, 1.0)
    }

    #[derive(Default)]
    struct MonitorChoice {
        secondary_work: Option<RECT>,
        primary_work: Option<RECT>,
        all_work_areas: Vec<RECT>,
    }

    /// 调试与设置卡优先出现在副屏；没有副屏才回退到主屏，避免占用用户工作区。
    fn preferred_window_geometry(
        desired_width: i32,
        desired_height: i32,
        margin: i32,
        prefer_secondary: bool,
    ) -> Option<(i32, i32, i32, i32)> {
        let monitors = monitor_work_areas()?;
        let work = if prefer_secondary {
            monitors.secondary_work.or(monitors.primary_work)
        } else {
            monitors.primary_work.or(monitors.secondary_work)
        }?;
        Some(fit_window_to_work_area(work, desired_width, desired_height, margin))
    }

    fn monitor_work_areas() -> Option<MonitorChoice> {
        let mut monitors = MonitorChoice::default();
        let data = LPARAM((&mut monitors as *mut MonitorChoice) as isize);
        unsafe { EnumDisplayMonitors(None, None, Some(collect_monitor_work_area), data) }.as_bool().then_some(monitors)
    }

    fn rects_intersect(left: RECT, right: RECT) -> bool {
        left.left < right.right && left.right > right.left && left.top < right.bottom && left.bottom > right.top
    }

    fn selected_work_width(display: &str) -> Option<u32> {
        let monitors = monitor_work_areas()?;
        let work = match display {
            "secondary" => monitors.secondary_work.or(monitors.primary_work),
            "primary" => monitors.primary_work.or(monitors.secondary_work),
            _ => None,
        }?;
        u32::try_from((work.right - work.left).max(1)).ok()
    }

    /// 避让值动态跟随当前目标显示器的可用宽度，上限为该宽度的一半。
    fn traffic_offset_limit(display: &str) -> Option<i32> {
        Some(traffic_offset_limit_for_work_width(selected_work_width(display)?))
    }

    fn traffic_offset_limit_for_work_width(work_width: u32) -> i32 {
        i32::try_from(u64::from(work_width) / 2).unwrap_or(i32::MAX)
    }

    /// 重新唤醒已打开的设置窗口。只有窗口落在所有当前工作区之外时才重新居中，
    /// 用户主动拖到副屏某处后再次点击设置不会无故跳回中心。
    pub(super) fn activate_existing_window(raw_hwnd: isize) {
        if raw_hwnd == 0 {
            return;
        }
        let hwnd = HWND(raw_hwnd as *mut _);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return;
        }
        let mut current = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut current) }.is_ok()
            && let Some(monitors) = monitor_work_areas()
            && !monitors.all_work_areas.iter().any(|work| rects_intersect(current, *work))
            && let Some(work) = monitors.secondary_work.or(monitors.primary_work)
        {
            let dpi = unsafe { GetDpiForSystem() }.max(96);
            let margin = scale_dip(16, dpi);
            let (left, top, _, _) =
                fit_window_to_work_area(work, current.right - current.left, current.bottom - current.top, margin);
            let _ = unsafe { SetWindowPos(hwnd, None, left, top, 0, 0, SWP_NOSIZE | SWP_NOZORDER) };
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        }
    }

    fn fit_window_to_work_area(
        work: RECT,
        desired_width: i32,
        desired_height: i32,
        margin: i32,
    ) -> (i32, i32, i32, i32) {
        let available_width = (work.right - work.left).max(1);
        let available_height = (work.bottom - work.top).max(1);
        let safe_margin = margin.max(0).min(available_width / 4).min(available_height / 4);
        let width = desired_width.clamp(1, (available_width - safe_margin * 2).max(1));
        let height = desired_height.clamp(1, (available_height - safe_margin * 2).max(1));
        let left = work.left + (available_width - width) / 2;
        let top = work.top + (available_height - height) / 2;
        (left, top, width, height)
    }

    unsafe extern "system" fn collect_monitor_work_area(
        monitor: HMONITOR,
        _device_context: HDC,
        _monitor_rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let Some(choice) = (unsafe { (data.0 as *mut MonitorChoice).as_mut() }) else { return BOOL(0) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return BOOL(1);
        }
        if info.dwFlags & MONITORINFOF_PRIMARY == 0 {
            choice.secondary_work.get_or_insert(info.rcWork);
        } else {
            choice.primary_work = Some(info.rcWork);
        }
        choice.all_work_areas.push(info.rcWork);
        BOOL(1)
    }

    fn create_webview(
        hwnd: HWND,
        document: String,
        settings_path: std::path::PathBuf,
        state: Arc<Mutex<AppConfig>>,
        actions: std::sync::mpsc::Sender<SettingsAction>,
        zoom_factor: f64,
    ) -> Result<SettingsWindowContext, String> {
        let environment = create_webview_environment().map_err(|error| error.to_string())?;
        let controller = create_webview_controller(&environment, hwnd).map_err(|error| error.to_string())?;
        let controller2 = controller.cast::<ICoreWebView2Controller2>().map_err(|error| error.to_string())?;
        unsafe {
            // 文档的 shell 填满无框窗口；A=0 避免 shell 圆角外出现黑色矩形。
            controller2
                .SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 })
                .map_err(|error| error.to_string())?;
        }
        let webview = unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
        unsafe {
            let settings = webview.Settings().map_err(|error| error.to_string())?;
            settings.SetAreDefaultContextMenusEnabled(false).map_err(|error| error.to_string())?;
            settings.SetAreDevToolsEnabled(false).map_err(|error| error.to_string())?;
            settings.SetIsZoomControlEnabled(false).map_err(|error| error.to_string())?;
        }
        install_settings_bridge(&webview, hwnd, settings_path, state, actions)?;
        let context = SettingsWindowContext { controller };
        resize_webview(hwnd, &context);
        unsafe {
            context.controller.SetZoomFactor(zoom_factor).map_err(|error| error.to_string())?;
            context.controller.SetIsVisible(true).map_err(|error| error.to_string())?;
            webview.NavigateToString(&HSTRING::from(document)).map_err(|error| error.to_string())?;
        }
        Ok(context)
    }

    fn install_settings_bridge(
        webview: &ICoreWebView2,
        hwnd: HWND,
        settings_path: std::path::PathBuf,
        state: Arc<Mutex<AppConfig>>,
        actions: std::sync::mpsc::Sender<SettingsAction>,
    ) -> Result<(), String> {
        let handler = WebMessageReceivedEventHandler::create(Box::new(move |webview, args| {
            let (Some(webview), Some(args)) = (webview, args) else { return Ok(()) };
            let mut raw = PWSTR(ptr::null_mut());
            if unsafe { args.WebMessageAsJson(&mut raw) }.is_err() {
                return Ok(());
            }
            let raw = CoTaskMemPWSTR::from(raw);
            let message = serde_json::from_str::<WebMessage>(&raw.to_string()).ok();
            match message {
                Some(WebMessage { action, .. }) if action == "close-settings" => {
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                Some(WebMessage { action, phase, screen_x, screen_y, .. }) if action == "drag-settings" => {
                    if phase.as_deref() == Some("start") {
                        // 交给 Windows 原生的窗口移动循环处理 DPI、跨屏和负坐标。
                        // WebView 只负责声明标题区按下，不再用 CSS screenX/Y 手算。
                        let _ = (screen_x, screen_y);
                        unsafe {
                            let _ = ReleaseCapture();
                            SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
                        }
                    }
                }
                Some(WebMessage { action, settings: Some(settings), .. }) if action == "save-settings" => {
                    let payload = match apply_layout_settings(&state, settings, &settings_path) {
                        Ok(()) => {
                            serde_json::json!({"kind":"settings-result","ok":true,"message":"已保存，任务栏将在一秒内应用。"})
                        }
                        Err(error) => {
                            serde_json::json!({"kind":"settings-result","ok":false,"message":error})
                        }
                    };
                    if let Ok(payload) = serde_json::to_string(&payload) {
                        let _ = unsafe { webview.PostWebMessageAsJson(&HSTRING::from(payload)) };
                    }
                }
                Some(WebMessage { action, .. }) => {
                    let requested = match action.as_str() {
                        "manual-refresh" => Some((SettingsAction::ManualRefresh, "已请求一次官方刷新。")),
                        "show-history" => Some((SettingsAction::ShowHistory, "已打开详情卡，可查看本机历史趋势。")),
                        "clear-history" => Some((SettingsAction::ClearHistory, "已提交本机历史清理。")),
                        "check-updates" => Some((SettingsAction::CheckUpdates, "已开始检查更新。")),
                        "download-update" => Some((SettingsAction::DownloadUpdate, "已提交更新下载与安装。")),
                        "export-diagnostics" => {
                            Some((SettingsAction::ExportDiagnostics, "已提交生成；完成后会自动打开诊断目录。"))
                        }
                        _ => None,
                    };
                    if let Some((requested, success_message)) = requested {
                        let sent = actions.send(requested).is_ok();
                        let payload = serde_json::json!({
                            "kind":"settings-result",
                            "ok":sent,
                            "message": if sent { success_message } else { "主运行时暂不可用，请重新打开软件后重试。" }
                        });
                        if let Ok(payload) = serde_json::to_string(&payload) {
                            let _ = unsafe { webview.PostWebMessageAsJson(&HSTRING::from(payload)) };
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }));
        let mut token = 0_i64;
        unsafe { webview.add_WebMessageReceived(&handler, &mut token).map_err(|error| error.to_string())? };
        Ok(())
    }

    fn apply_layout_settings(
        current: &Arc<Mutex<AppConfig>>,
        input: LayoutSettings,
        settings_path: &std::path::Path,
    ) -> Result<(), String> {
        let mut guard = current.lock().map_err(|_| "设置状态不可用".to_owned())?;
        let mut next = guard.clone();
        // “主屏/副屏”是自动选择策略，不保留可能已经断开的旧设备名。
        next.target_monitor_device = None;
        let requested_secondary = match input.display.as_str() {
            "secondary" => true,
            "primary" => false,
            _ => return Err("显示器选择无效".to_owned()),
        };
        let has_secondary = monitor_work_areas().is_some_and(|monitors| monitors.secondary_work.is_some());
        // 副屏可能只是暂时断开或被系统停用。保存用户的自动副屏偏好，当前无副屏时
        // 仅使用主屏工作区校验布局；副屏恢复后常驻程序即可自动迁回。
        next.prefer_secondary_monitor = requested_secondary;
        let effective_display = if requested_secondary && has_secondary { "secondary" } else { "primary" };
        next.anchor = match input.dock.as_str() {
            "left" => TaskbarAnchor::Left,
            "right" => TaskbarAnchor::Right,
            _ => return Err("停靠方向无效".to_owned()),
        };
        let width = u32::try_from(input.width).map_err(|_| "宽度超出范围".to_owned())?;
        if !(MIN_TASKBAR_WIDTH_PX..=MAX_TASKBAR_WIDTH_PX).contains(&width) {
            return Err(format!("宽度必须在 {MIN_TASKBAR_WIDTH_PX}–{MAX_TASKBAR_WIDTH_PX}px 之间"));
        }
        let traffic = i32::try_from(input.traffic).map_err(|_| "避让像素超出范围".to_owned())?;
        let traffic_max =
            traffic_offset_limit(effective_display).ok_or_else(|| "无法读取目标屏幕可用空间".to_owned())?;
        if !(0..=traffic_max).contains(&traffic) {
            return Err(format!("避让像素必须在 0–{traffic_max}px 之间"));
        }
        next.taskbar_width_px = width;
        next.traffic_monitor_offset_px = traffic;
        next.taskbar_background_opacity_percent =
            u8::try_from(input.opacity).map_err(|_| "透明度超出范围".to_owned())?;
        next.log_level = LogLevel::from_str(&input.log_level).map_err(|_| "日志等级无效".to_owned())?;
        next.reduce_motion = input.reduce_motion;
        next.sync_mode = match input.sync_mode.as_str() {
            "smart" => SyncMode::Smart,
            "economy" => SyncMode::Economy,
            _ => return Err("同步策略无效".to_owned()),
        };
        next.history_retention_days =
            u16::try_from(input.history_retention_days).map_err(|_| "历史保留时长超出范围".to_owned())?;
        next.log_retention_days =
            u16::try_from(input.log_retention_days).map_err(|_| "日志保留时长超出范围".to_owned())?;
        next.adaptive_chunk_download = input.adaptive_chunk_download;
        let next = next.normalize();
        next.save_atomic(settings_path).map_err(|_| "写入设置失败".to_owned())?;
        let persisted = AppConfig::load(settings_path).map_err(|_| "设置已写入但回读校验失败".to_owned())?;
        if persisted != next {
            return Err("设置回读结果不一致，请重试".to_owned());
        }
        request_reload(settings_path).map_err(|_| "通知任务栏重载失败".to_owned())?;
        *guard = persisted;
        Ok(())
    }

    fn resize_webview(hwnd: HWND, context: &SettingsWindowContext) {
        let mut bounds = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut bounds) }.is_ok() {
            let _ = unsafe { context.controller.SetBounds(bounds) };
        }
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut SettingsWindowContext;
        match message {
            WM_SIZE if !raw.is_null() => unsafe { resize_webview(hwnd, &*raw) },
            WM_DESTROY => {
                if !raw.is_null() {
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                        drop(Box::from_raw(raw));
                    }
                }
                unsafe { PostQuitMessage(0) };
            }
            _ => {}
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn create_webview_environment() -> Result<ICoreWebView2Environment, webview2_com::Error> {
        let (sender, receiver) = sync_channel(1);
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(|handler| unsafe {
                CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, environment| {
                result?;
                let _ = sender.send(environment.ok_or_else(|| Error::from(E_INVALIDARG)));
                Ok(())
            }),
        )?;
        Ok(receiver.recv().map_err(|_| webview2_com::Error::WindowsError(Error::from(E_INVALIDARG)))??)
    }

    fn create_webview_controller(
        environment: &ICoreWebView2Environment,
        parent: HWND,
    ) -> Result<ICoreWebView2Controller, webview2_com::Error> {
        let (sender, receiver) = sync_channel(1);
        let environment = environment.clone();
        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                environment.CreateCoreWebView2Controller(parent, &handler).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, controller| {
                result?;
                let _ = sender.send(controller.ok_or_else(|| Error::from(E_INVALIDARG)));
                Ok(())
            }),
        )?;
        Ok(receiver.recv().map_err(|_| webview2_com::Error::WindowsError(Error::from(E_INVALIDARG)))??)
    }

    fn settings_web_document(config: &AppConfig) -> String {
        let monitors = monitor_work_areas().unwrap_or_default();
        let work_width = |work: Option<RECT>| work.map_or(0, |rect| (rect.right - rect.left).max(0));
        let has_secondary = monitors.secondary_work.is_some();
        let snapshot = serde_json::json!({
            "display": if config.prefer_secondary_monitor { "secondary" } else { "primary" },
            "has_secondary": has_secondary,
            "dock": if config.anchor == TaskbarAnchor::Left { "left" } else { "right" },
            "width": config.taskbar_width_px,
            "traffic": config.traffic_monitor_offset_px.max(0),
            "width_min": MIN_TASKBAR_WIDTH_PX,
            "width_max": MAX_TASKBAR_WIDTH_PX,
            "primary_work_width": work_width(monitors.primary_work),
            "secondary_work_width": work_width(monitors.secondary_work),
            "opacity": config.taskbar_background_opacity_percent,
            "log_level": config.log_level.as_str(),
            "reduce_motion": config.reduce_motion,
            "sync_mode": match config.sync_mode { SyncMode::Smart => "smart", SyncMode::Economy => "economy" },
            "history_retention_days": config.history_retention_days,
            "log_retention_days": config.log_retention_days,
            "adaptive_chunk_download": config.adaptive_chunk_download,
        });
        let preview = TASKBAR_WEB_DOCUMENT
            .replacen(
                "<script src=\"taskbar-visual-contract.js\"></script>",
                &format!("<script>{TASKBAR_VISUAL_CONTRACT}</script>"),
                1,
            )
            .replacen("<html", "<html class=\"embed\"", 1);
        let bridge = format!(
            "<script>window.__CodexTaskbarSettingsEmbed=true;window.__CodexTaskbarSettingsSnapshot={};window.__CodexTaskbarPreviewDocument={};</script>",
            json_value_for_script(&snapshot),
            json_for_script(&preview),
        );
        SETTINGS_WEB_DOCUMENT.replacen("<html", "<html class=\"settings-embed\"", 1).replacen(
            "</head>",
            &format!("{bridge}</head>"),
            1,
        )
    }

    /// 防止内嵌预览中的 `</script>` 结束外层桥接脚本。
    fn json_for_script(value: &str) -> String {
        // 先得到 JavaScript 字符串字面量，再替换字面量中的 HTML 终止字符。若在
        // 序列化前替换，serde 会再次转义反斜杠，iframe 就会把 `\\u003c` 当文字。
        serde_json::to_string(value)
            .expect("字符串总能序列化")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
    }

    /// 设置快照必须作为 JavaScript 对象注入。若先转成字符串，页面读取
    /// `width`、`has_secondary` 等字段时都会得到 undefined 并回退到演示默认值。
    fn json_value_for_script(value: &serde_json::Value) -> String {
        value.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[test]
        fn embedded_document_receives_every_persisted_settings_page_value() {
            let config = AppConfig {
                sync_mode: SyncMode::Economy,
                history_retention_days: 365,
                log_retention_days: 7,
                adaptive_chunk_download: false,
                ..AppConfig::default()
            };
            let document = settings_web_document(&config);
            assert!(document.contains("settings-embed"));
            assert!(document.contains("\"sync_mode\":\"economy\""));
            assert!(document.contains("\"history_retention_days\":365"));
            assert!(document.contains("\"log_retention_days\":7"));
            assert!(document.contains("\"adaptive_chunk_download\":false"));
            assert!(document.contains("widthRange"));
            assert!(document.contains("trafficRange"));
            assert!(document.contains("has_secondary"));
            assert!(document.contains("\"width_min\":200"));
            assert!(document.contains("\"width_max\":620"));
            assert!(document.contains("window.__CodexTaskbarSettingsSnapshot={"));
            assert!(!document.contains("window.__CodexTaskbarSettingsSnapshot=\"{"));
        }

        #[test]
        fn one_save_payload_updates_layout_sync_and_diagnostics_atomically() {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let directory = std::env::temp_dir().join(format!("codex-taskbar-settings-ui-{nonce}"));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("settings.json");
            let state = Arc::new(Mutex::new(AppConfig::default()));
            apply_layout_settings(
                &state,
                LayoutSettings {
                    display: "secondary".to_owned(),
                    dock: "right".to_owned(),
                    width: 420,
                    traffic: 14,
                    opacity: 64,
                    log_level: "info".to_owned(),
                    reduce_motion: false,
                    sync_mode: "economy".to_owned(),
                    history_retention_days: 365,
                    log_retention_days: 7,
                    adaptive_chunk_download: false,
                },
                &path,
            )
            .unwrap();
            let saved = AppConfig::load(&path).unwrap();
            assert_eq!(saved.sync_mode, SyncMode::Economy);
            assert_eq!(saved.history_retention_days, 365);
            assert_eq!(saved.log_retention_days, 7);
            assert!(!saved.adaptive_chunk_download);
            assert!(saved.prefer_secondary_monitor);
            let reopened = settings_web_document(&saved);
            assert!(reopened.contains("\"display\":\"secondary\""));
            assert!(reopened.contains("\"width\":420"));
            assert!(reopened.contains("\"traffic\":14"));
            std::fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn settings_card_is_clamped_inside_the_secondary_work_area() {
            let work = RECT { left: -1920, top: 0, right: 0, bottom: 1040 };
            let (left, top, width, height) = fit_window_to_work_area(work, 1650, 1350, 24);
            assert_eq!((left, top, width, height), (-1785, 24, 1650, 992));
            assert!(left >= work.left && left + width <= work.right);
            assert!(top >= work.top && top + height <= work.bottom);
        }

        #[test]
        fn settings_zoom_fits_the_full_card_at_high_dpi() {
            let zoom = settings_zoom_factor(1650, 992, 144);
            assert!((zoom - 0.768_99).abs() < 0.001);
            assert_eq!(settings_zoom_factor(1650, 1350, 144), 1.0);
        }

        #[test]
        fn existing_window_visibility_and_manual_offset_limit_use_work_area() {
            let primary = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
            let disconnected_secondary = RECT { left: -1920, top: 0, right: -900, bottom: 900 };
            assert!(rects_intersect(primary, RECT { left: 120, top: 80, right: 900, bottom: 700 }));
            assert!(!rects_intersect(primary, disconnected_secondary));
            assert_eq!(traffic_offset_limit_for_work_width(1920), 960);
            assert_eq!(traffic_offset_limit_for_work_width(1), 0);
        }
    }
}

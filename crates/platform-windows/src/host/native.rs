//! Windows HWND 消息循环实现；仅由 `host` 模块的线程安全接口调用。

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicIsize, Ordering},
        mpsc::{Receiver, channel, sync_channel},
    },
    thread,
};

use webview2_com::{
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::*, WebMessageReceivedEventHandler,
};
use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateRoundRectRgn, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MonitorFromWindow, PAINTSTRUCT, SetWindowRgn,
        },
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI,
                SetThreadDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
            },
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateIcon, CreatePopupMenu, CreateWindowExW,
                DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, EnumWindows, GWL_STYLE,
                GWLP_USERDATA, GetClassNameW, GetClientRect, GetCursorPos, GetMessageW, GetParent, GetWindowLongPtrW,
                GetWindowRect, HICON, HWND_TOP, HWND_TOPMOST, IDC_ARROW, IsWindow, LoadCursorW, MF_SEPARATOR,
                MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
                SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetForegroundWindow, SetParent, SetTimer,
                SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
                TranslateMessage, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED,
                WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NULL, WM_PAINT,
                WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_POPUP,
            },
        },
    },
    core::{BOOL, Error, HSTRING, Interface, PCWSTR, PWSTR, w},
};

use crate::{
    PlatformError,
    geometry::PixelRect,
    host::{
        HostRuntime, NativeHostCommand, NativeHostCommandError, NativeHostConfig, NativeHostDetails, NativeHostEvent,
        NativeHostHandle, NativeHostModel, NativeNotification, NativeNotificationKind, TaskbarParent,
    },
    render::{DetailsAction, Direct2dRenderer, details_action_hit_test, details_trend_hit_test},
    render_model::DipRect,
};

const CLASS_NAME: PCWSTR = w!("CodexTaskbarNativeHost");
const DETAILS_CLASS_NAME: PCWSTR = w!("CodexTaskbarDetailsCard");
const COMMAND_MESSAGE: u32 = WM_APP + 41;
const EXIT_MESSAGE: u32 = WM_APP + 42;
const DETAILS_CLOSED_MESSAGE: u32 = WM_APP + 43;
const TOKEN_STRIP_CLOSED_MESSAGE: u32 = WM_APP + 44;
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 45;
const DETAILS_REFRESH_REQUESTED_MESSAGE: u32 = WM_APP + 46;
const DETAILS_SETTINGS_REQUESTED_MESSAGE: u32 = WM_APP + 47;
/// 详情 WebView2 DOM 与消息监听器就绪后请求重投当前脱敏快照，避免首次导航
/// 尚未完成时丢掉创建窗口阶段的 PostWebMessageAsJson。
const DETAILS_WEB_READY_MESSAGE: u32 = WM_APP + 48;
/// Token 快览页的 DOM/消息监听器就绪。与详情卡相同，必须由网页主动请求重投
/// 快照，不能假设 NavigateToString 完成前的 WebMessage 会被 WebView2 缓存。
const TOKEN_STRIP_WEB_READY_MESSAGE: u32 = WM_APP + 49;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
const ANIMATION_TIMER_ID: usize = 1;
/// 流场始终以约 60 FPS 更新。小胶囊只重绘自身的脏区，不访问网络或磁盘；连续
/// 相位是“新一层浪从上方滚下”的基础，不能再依赖状态切换时的短暂动画。
const FRAME_MS: u32 = 16;
/// Token 快览是由新 turn/token 事件触发的短提示；每次触发都会重置这个
/// 一次性计时器，避免悬浮层在数据更新后一直占据任务栏上方空间。
const TOKEN_STRIP_AUTO_HIDE_TIMER_ID: usize = 2;
const TOKEN_STRIP_AUTO_HIDE_MS: u32 = 4_000;
const DETAILS_OUTSIDE_CLICK_TIMER_ID: usize = 3;
const DETAILS_OUTSIDE_CLICK_POLL_MS: u32 = 16;
const TRAY_ICON_ID: u32 = 1;
const MENU_SHOW_DETAILS: usize = 1_001;
const MENU_OPEN_SETTINGS: usize = 1_002;
const MENU_RELOAD_SETTINGS: usize = 1_003;
const MENU_OPEN_CONFIG_DIR: usize = 1_004;
const MENU_EDIT_SETTINGS: usize = 1_005;
const MENU_OPEN_LOG_DIR: usize = 1_006;
const MENU_EXIT: usize = 1_007;

/// 已确认的 WebGL 视觉稿。运行时将外部脚本内联并强制 `embed` 模式，避免
/// `NavigateToString` 没有文件基址时丢失资源，也避免把设计稿的测试控件带进
/// Explorer 任务栏。
const TASKBAR_WEB_DOCUMENT: &str = include_str!("../../../../prototypes/fluid-front-reference.html");
const TASKBAR_VISUAL_CONTRACT: &str = include_str!("../../../../prototypes/taskbar-visual-contract.js");
/// 已确认的详情卡视觉稿。生产环境用编译期内嵌版本，禁止加载外部 URL、字体或脚本。
const DETAILS_WEB_DOCUMENT: &str = include_str!("../../../../prototypes/details-card-reference.html");

pub(super) fn spawn(config: NativeHostConfig, model: NativeHostModel) -> Result<NativeHostHandle, PlatformError> {
    let (sender, receiver) = channel();
    let (event_sender, event_receiver) = channel();
    let wake = Arc::new(AtomicIsize::new(0));
    let thread_wake = Arc::clone(&wake);
    let (ready_sender, ready_receiver) = sync_channel(1);
    thread::Builder::new()
        .name("codex-taskbar-ui".to_owned())
        .spawn(move || run_loop(config, model, receiver, event_sender, thread_wake, ready_sender))
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    ready_receiver
        .recv()
        .map_err(|_| PlatformError::Windows("原生宿主在线程初始化前退出".to_owned()))?
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    Ok(NativeHostHandle { sender, events: Arc::new(std::sync::Mutex::new(event_receiver)), wake })
}

pub(super) fn wake_host(wake: &AtomicIsize) -> Result<(), NativeHostCommandError> {
    let raw = wake.load(Ordering::Acquire);
    if raw == 0 {
        return Err(NativeHostCommandError::Stopped);
    }
    // SAFETY: raw 只在窗口创建成功后发布，并在 WM_DESTROY 前清零；消息不携带 Rust 指针。
    unsafe { PostMessageW(Some(HWND(raw as *mut c_void)), COMMAND_MESSAGE, WPARAM(0), LPARAM(0)) }
        .map_err(|_| NativeHostCommandError::Stopped)
}

fn run_loop(
    config: NativeHostConfig,
    model: NativeHostModel,
    receiver: Receiver<NativeHostCommand>,
    event_sender: std::sync::mpsc::Sender<NativeHostEvent>,
    wake: Arc<AtomicIsize>,
    ready: std::sync::mpsc::SyncSender<Result<(), Error>>,
) {
    // WebView2 控制器必须在 STA 线程创建并在同一消息循环中使用。这个 UI 线程
    // 独占 HWND 生命周期，因而不会和采集线程共享 COM 对象。
    // 主进程可能已经被 Explorer 或其它库锁定为系统 DPI awareness。窗口线程可
    // 单独提升为 Per-Monitor V2，确保副屏上的 WebView2 以实际 DPI 创建 CSS
    // viewport，而不是把 920 DIP 的详情卡压缩成 460 CSS 像素。
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() } {
        let _ = ready.send(Err(with_stage(error, "初始化任务栏 WebView2 COM 公寓")));
        return;
    }
    let result = create_window(config, model, receiver, event_sender, wake);
    match result {
        Ok(hwnd) => {
            let _ = ready.send(Ok(()));
            let mut message = MSG::default();
            // SAFETY: 当前线程拥有该窗口与消息队列；MSG 是有效可写缓冲区。
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                // SAFETY: 仅转发由 GetMessageW 填充的消息。
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            let _ = hwnd;
            // 与上面的 CoInitializeEx 成对；此时所有 WebView2 COM 对象已经在
            // 当前线程析构完毕，不会把接口带到其它线程。
            unsafe { CoUninitialize() };
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            unsafe { CoUninitialize() };
        }
    }
}

fn create_window(
    config: NativeHostConfig,
    model: NativeHostModel,
    receiver: Receiver<NativeHostCommand>,
    event_sender: std::sync::mpsc::Sender<NativeHostEvent>,
    wake: Arc<AtomicIsize>,
) -> Result<HWND, Error> {
    register_class().map_err(|error| with_stage(error, "注册窗口类"))?;
    register_details_class().map_err(|error| with_stage(error, "注册详情卡片窗口类"))?;
    let context = Box::new(WindowContext {
        runtime: HostRuntime::new(model, tick_count_ms()),
        renderer: Direct2dRenderer::new().map_err(|error| with_stage(error, "创建 Direct2D/DirectWrite 工厂"))?,
        taskbar_webview: None,
        commands: receiver,
        events: event_sender,
        wake,
        visible: config.initially_visible,
        timer_running: false,
        details: NativeHostDetails::default(),
        token_strip_snapshot: None,
        details_web_snapshot: None,
        details_window: None,
        token_strip_window: None,
        tracking_mouse: false,
        tray_icon_registered: false,
        tray_icon: None,
        taskbar_parent: config.taskbar_parent,
    });
    // TrafficMonitor 使用独立顶层窗口覆盖 Explorer 任务栏；仅靠用户手填偏移
    // 无法适配它随显示器/DPI 改变的位置。启动时只读枚举其可见仪表窗口，若与
    // 本胶囊重叠则在同一任务栏内避让。找不到或空间不足时保持用户的原始布局。
    let rect = avoid_traffic_monitor_overlap(config.rect, config.taskbar_parent);
    let parent = hwnd_from_raw(config.taskbar_parent.hwnd)?;
    let client_rect = screen_to_taskbar_client(rect, config.taskbar_parent)?;
    // SAFETY: 先建立本进程 top-level layered window，再按 TrafficMonitor 的方式
    // SetParent；Windows 11 Explorer 会拒绝直接以跨进程 parent 创建 WS_CHILD。
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("Codex Taskbar"),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            None,
            None,
            None,
            Some(Box::into_raw(context).cast()),
        )
        .map_err(|error| with_stage(error, "创建原生 HWND"))?
    };
    if hwnd.0.is_null() {
        return Err(Error::new(E_INVALIDARG, "创建 layered HWND 返回空句柄"));
    }
    if let Err(error) = attach_window(hwnd, parent) {
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    // WebView2 只负责任务栏主胶囊；详情卡与 Token 快览仍由既有 native popup
    // 承担，后续在各自独立阶段迁移。创建失败时保留 Direct2D 作为安全降级，
    // 不让缺少 WebView2 Runtime 导致整个监控程序无法启动。
    let context_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext };
    if !context_ptr.is_null() {
        match TaskbarWebView::create(hwnd, rect.width()) {
            Ok(webview) => {
                unsafe { (*context_ptr).taskbar_webview = Some(webview) };
                tracing::info!(event = "taskbar_webview_ready", "WebView2/WebGL 任务栏渲染器已就绪");
            }
            Err(error) => tracing::warn!(
                event = "taskbar_webview_fallback",
                error = %error,
                "WebView2 任务栏渲染器不可用，已回退原生渲染"
            ),
        }
    }
    if config.initially_visible {
        // SAFETY: hwnd 是任务栏的无激活 child；HWND_TOP 只调整同一父窗口内的
        // sibling Z-order，不会成为覆盖其它应用的全局 TOPMOST 窗口。
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                client_rect.left,
                client_rect.top,
                client_rect.width(),
                client_rect.height(),
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        };
    }
    // SAFETY: 此时窗口仍有效，WM_NCCREATE 已将 wake 的 HWND 值发布。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    };
    // 托盘图标是任务栏嵌入失败或窗口暂时不可见时仍可到达的控制入口。
    let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext };
    if !context.is_null() {
        unsafe {
            (*context).tray_icon = add_tray_icon(hwnd);
            (*context).tray_icon_registered = (*context).tray_icon.is_some();
        };
    }
    Ok(hwnd)
}

fn register_class() -> Result<(), Error> {
    // SAFETY: 使用当前模块句柄；不保存 Rust 字符串指针。
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        // SAFETY: 系统 IDC_ARROW 是静态资源标识；返回句柄由系统管理。
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: class 在调用期间有效；重复注册会返回 0，随后 CreateWindowExW 仍会使用已注册类。
    let atom = unsafe { RegisterClassExW(&class) };
    let _ = atom;
    Ok(())
}

fn register_details_class() -> Result<(), Error> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(details_window_proc),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: DETAILS_CLASS_NAME,
        ..Default::default()
    };
    let _ = unsafe { RegisterClassExW(&class) };
    Ok(())
}

fn with_stage(error: Error, stage: &str) -> Error {
    Error::new(error.code(), format!("{stage}失败：{error}"))
}

struct WindowContext {
    runtime: HostRuntime,
    renderer: Direct2dRenderer,
    /// 真正的任务栏本体使用 WebView2/WebGL。`None` 代表 Runtime 不可用时的
    /// Direct2D 安全降级；绝不在运行时高频创建或销毁 Chromium 控制器。
    taskbar_webview: Option<TaskbarWebView>,
    commands: Receiver<NativeHostCommand>,
    events: std::sync::mpsc::Sender<NativeHostEvent>,
    wake: Arc<AtomicIsize>,
    visible: bool,
    timer_running: bool,
    details: NativeHostDetails,
    /// 最近一次安全 Token 增量快照。仅在自动弹窗的短生命周期内保留内存，
    /// 不写日志、不落盘；窗口创建完成后重新投递以避免导航首帧丢消息。
    token_strip_snapshot: Option<Box<str>>,
    /// 最近一次详情展示快照。它只含 UI 文本、图表数值与安全标签，供新打开的
    /// 详情 WebView2 补发首帧；不写入磁盘或诊断日志。
    details_web_snapshot: Option<Box<str>>,
    details_window: Option<HWND>,
    token_strip_window: Option<HWND>,
    tracking_mouse: bool,
    tray_icon_registered: bool,
    /// Shell_NotifyIcon 不拥有该句柄；Explorer 重挂接和退出时必须显式释放。
    tray_icon: Option<HICON>,
    taskbar_parent: TaskbarParent,
}

/// Explorer 任务栏子窗口内的 WebView2 控制器。
///
/// 页面来自编译期内嵌的本地静态字符串，禁用开发者工具、缩放与右键菜单；主进程
/// 只通过 `PostWebMessageAsJson` 推送已经脱敏的展示快照，不开放页面导航或任意
/// JavaScript 执行通道。
struct TaskbarWebView {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}

impl TaskbarWebView {
    fn create(parent: HWND, physical_width_px: i32) -> Result<Self, String> {
        Self::create_with_document(parent, taskbar_web_document(physical_width_px))
    }

    /// 用同一受限 WebView2 配置承载短生命周期的玻璃弹窗。调用方只能给出
    /// 编译期内嵌的本地文档，不能把 URL 或动态脚本传进渲染进程。
    fn create_with_document(parent: HWND, document: String) -> Result<Self, String> {
        let environment = create_webview_environment().map_err(|error| error.to_string())?;
        let controller = create_webview_controller(&environment, parent).map_err(|error| error.to_string())?;
        let controller2 = controller.cast::<ICoreWebView2Controller2>().map_err(|error| error.to_string())?;
        // `A=0` 才能让 HTML/WebGL 输出的透明像素露出下面的 Explorer 任务栏；
        // 玻璃效果由网页内的半透明 CSS/WebGL 输出完成，WebView2 本身不接受
        // 半透明默认背景色。
        unsafe {
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
        install_web_action_bridge(&webview, parent)?;
        let mut bounds = RECT::default();
        unsafe {
            GetClientRect(parent, &mut bounds).map_err(|error| error.to_string())?;
            controller.SetBounds(bounds).map_err(|error| error.to_string())?;
            controller.SetIsVisible(true).map_err(|error| error.to_string())?;
            webview.NavigateToString(&HSTRING::from(document)).map_err(|error| error.to_string())?;
        }
        Ok(Self { controller, webview })
    }

    fn resize(&self, parent: HWND) {
        let mut bounds = RECT::default();
        // 窗口大小、DPI 或 Explorer parent 变化时只更新控制器 bounds；不会重载
        // 页面，因此 WebGL 的 requestAnimationFrame 相位保持连续。
        if unsafe { GetClientRect(parent, &mut bounds) }.is_ok() {
            if let Err(error) = unsafe { self.controller.SetBounds(bounds) } {
                tracing::debug!(event = "taskbar_webview_resize_failed", error = %error, "无法更新 WebView2 尺寸");
            }
        }
    }

    fn post_snapshot(&self, json: &str) {
        // 输入来自应用层 TaskbarSnapshot 的 serde JSON；平台层没有拼接脚本，不会
        // 让数据内容被解释为 JavaScript。网页同样校验 schema_version 后才使用。
        if let Err(error) = unsafe { self.webview.PostWebMessageAsJson(&HSTRING::from(json)) } {
            tracing::debug!(event = "taskbar_webview_snapshot_dropped", error = %error, "无法投递任务栏展示快照");
        }
    }
}

/// 允许本地内嵌页面发出三个固定 UI 意图。网页无法传入消息号、URL 或任意
/// 命令；桥接层也不解析或记录其余字段，避免 WebView2 成为通用控制通道。
fn install_web_action_bridge(webview: &ICoreWebView2, owner: HWND) -> Result<(), String> {
    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_webview, args| {
        let Some(args) = args else { return Ok(()) };
        let mut raw = PWSTR(ptr::null_mut());
        if unsafe { args.WebMessageAsJson(&mut raw) }.is_err() {
            return Ok(());
        }
        let raw = CoTaskMemPWSTR::from(raw);
        let payload = serde_json::from_str::<serde_json::Value>(&raw.to_string()).ok();
        let action = payload.as_ref().and_then(|value| value.get("action")?.as_str().map(str::to_owned));
        // 仅在 debug 构建记录无业务含义的窗口尺寸，用于定位 Explorer 的副屏
        // DPI 虚拟化。这里不记录任何账户、额度、Token 或页面文本。
        if cfg!(debug_assertions) && action.as_deref() == Some("details-layout-diagnostics") {
            let dimension = |name: &str| payload.as_ref().and_then(|value| value.get(name)?.as_u64()).unwrap_or(0);
            tracing::debug!(
                event = "details_layout_diagnostics",
                viewport_width = dimension("viewport_width"),
                viewport_height = dimension("viewport_height"),
                shell_width = dimension("shell_width"),
                grid_width = dimension("grid_width"),
                primary_width = dimension("primary_width"),
                secondary_width = dimension("secondary_width"),
                "详情卡已上报脱敏布局尺寸"
            );
        }
        let message = web_action_message(action.as_deref());
        if let Some(message) = message {
            let _ = unsafe { PostMessageW(Some(owner), message, WPARAM(0), LPARAM(0)) };
        }
        Ok(())
    }));
    let mut token = 0_i64;
    unsafe { webview.add_WebMessageReceived(&handler, &mut token).map_err(|error| error.to_string())? };
    Ok(())
}

/// WebView2 只允许向原生层表达这些固定 UI 意图；不能把网页输入变成任意
/// Win32 消息。任务栏正文覆盖 HWND 后，右键必须由这里显式转回宿主菜单。
fn web_action_message(action: Option<&str>) -> Option<u32> {
    match action {
        Some("show-details") => Some(WM_LBUTTONUP),
        Some("show-menu") => Some(WM_CONTEXTMENU),
        Some("refresh-details") => Some(DETAILS_REFRESH_REQUESTED_MESSAGE),
        Some("open-settings") => Some(DETAILS_SETTINGS_REQUESTED_MESSAGE),
        Some("details-ready") => Some(DETAILS_WEB_READY_MESSAGE),
        Some("token-strip-ready") => Some(TOKEN_STRIP_WEB_READY_MESSAGE),
        _ => None,
    }
}

fn create_webview_environment() -> std::result::Result<ICoreWebView2Environment, webview2_com::Error> {
    // Never let WebView2 create an .exe.WebView2 directory beside a desktop executable.
    let data_dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("CodexTaskbar")
        .join("WebView2");
    let data_dir = windows::core::HSTRING::from(data_dir.as_os_str());
    let (sender, receiver) = sync_channel(1);
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                windows::core::PCWSTR::null(),
                &data_dir,
                None::<&ICoreWebView2EnvironmentOptions>,
                &handler,
            )
            .map_err(webview2_com::Error::WindowsError)
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
) -> std::result::Result<ICoreWebView2Controller, webview2_com::Error> {
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

fn taskbar_web_document(physical_width_px: i32) -> String {
    // 外部视觉契约内联后，生产嵌入页没有文件基址依赖；强制 `embed` 模式隐藏
    // 设计工具控件，只保留 62px 高的真实胶囊。
    let document = TASKBAR_WEB_DOCUMENT.replacen(
        "<script src=\"taskbar-visual-contract.js\"></script>",
        &format!("<script>{TASKBAR_VISUAL_CONTRACT}</script>"),
        1,
    );
    document.replacen("<html", "<html class=\"embed\"", 1).replacen(
        "</head>",
        &format!("<script>window.__CodexTaskbarPhysicalWidth={};</script></head>", physical_width_px.clamp(200, 620)),
        1,
    )
}

/// 生成本次消耗浮窗的受限 WebView 文档。
///
/// 箭头位置只使用已夹紧的窗口几何值，不能也不会包含用户数据；这样 popup
/// 因工作区边界被平移时，箭头仍精准指向真实任务栏胶囊的中心。
fn token_strip_web_document(arrow_x_dip: f32) -> String {
    let document = TASKBAR_WEB_DOCUMENT.replacen(
        "<script src=\"taskbar-visual-contract.js\"></script>",
        &format!("<script>{TASKBAR_VISUAL_CONTRACT}</script>"),
        1,
    );
    let document = document.replacen("<html", "<html class=\"consume-embed\"", 1);
    document.replacen(
        "</head>",
        &format!("<style>html.consume-embed .consume-popover{{--arrow-x:{arrow_x_dip:.1}px}}</style></head>"),
        1,
    )
}

/// 详情卡与已确认原型共用一套 HTML/CSS；仅通过 WebMessage 提交业务层已经
/// 脱敏、格式化的字段。`details-embed` 会删除演示选择器及模拟说明。
fn details_card_web_document(_monitor_scale: f32) -> String {
    // 此处先使用未缩放文档，让 HTML 自身按 WebView2 实际 CSS 视口响应；布局
    // 诊断会验证 1+4 网格真实宽度，再决定是否需要原生层面而非 CSS 的 DPI 修正。
    DETAILS_WEB_DOCUMENT.replacen("<html", "<html class=\"details-embed\"", 1)
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE 的 lparam 指向系统在调用期间提供的 CREATESTRUCTW；lpCreateParams 来自 Box::into_raw。
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        let context = create.lpCreateParams as *mut WindowContext;
        // SAFETY: 将唯一 Box 指针保存到本窗口的用户数据；之后只有本 WndProc 读取和释放它。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize) };
        // SAFETY: context 已由本窗口唯一拥有且在 WM_DESTROY 前有效。
        unsafe { (*context).wake.store(hwnd.0 as isize, Ordering::Release) };
    }
    // SAFETY: 仅读取上面在 WM_NCCREATE 保存的当前窗口指针。
    let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext };
    if context.is_null() {
        // SAFETY: 创建前的标准默认处理不访问 Rust 状态。
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if message == WM_DESTROY {
        // SAFETY: 指针由此 HWND 专有，消息循环在同一线程串行调用 WndProc。
        {
            let context = unsafe { &mut *context };
            stop_timer(hwnd, context);
            remove_tray_icon(hwnd, context);
            context.renderer.on_device_lost();
            context.wake.store(0, Ordering::Release);
        }
        // SAFETY: 清除窗口槽位后恰好释放 WM_NCCREATE 保存的 Box；之后本过程不再解引用它。
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(context));
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }
    // SAFETY: 指针由此 HWND 专有，消息循环在同一线程串行调用 WndProc。
    let context = unsafe { &mut *context };
    match message {
        COMMAND_MESSAGE => {
            process_commands(hwnd, context);
            LRESULT(0)
        }
        WM_PAINT => {
            if context.taskbar_webview.is_some() {
                // WebView2 自己提交 GPU 帧；这里只验证系统脏区，避免旧 Direct2D
                // 在透明区域补上一层黑底。
                let mut paint = PAINTSTRUCT::default();
                unsafe {
                    let _ = BeginPaint(hwnd, &mut paint);
                    let _ = EndPaint(hwnd, &paint);
                }
            } else {
                paint(hwnd, context);
            }
            LRESULT(0)
        }
        // layered surface 自己提交透明像素，禁止 DefWindowProc 用类背景擦除。
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            if !context.tracking_mouse {
                let mut event = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut event) }.is_ok() {
                    context.tracking_mouse = true;
                }
            }
            // Token 快览是“本次消耗”的自动反馈，不是 hover 提示。此前鼠标掠过
            // 任务栏也会创建一个没有 Token 快照的空弹窗，用户自然总是看到 --。
            if context.details_window.is_none() && context.token_strip_snapshot.is_some() {
                show_token_strip(hwnd, context);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MESSAGE => {
            context.tracking_mouse = false;
            close_token_strip(context);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            close_token_strip(context);
            toggle_details_card(hwnd, context);
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            close_token_strip(context);
            show_tray_menu(hwnd);
            LRESULT(0)
        }
        TRAY_CALLBACK_MESSAGE => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    close_token_strip(context);
                    toggle_details_card(hwnd, context);
                    emit_host_event(context, NativeHostEvent::ShowDetailsRequested);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => show_tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_menu_command(context, wparam.0 & 0xffff);
            LRESULT(0)
        }
        DETAILS_CLOSED_MESSAGE => {
            context.details_window = None;
            LRESULT(0)
        }
        TOKEN_STRIP_CLOSED_MESSAGE => {
            context.token_strip_window = None;
            // 自动收起后不保留旧一轮的 Token。否则下次仅鼠标掠过任务栏会把
            // 已结束的消费重新弹出，既像空弹窗也会误导为刚发生了新消耗。
            context.token_strip_snapshot = None;
            LRESULT(0)
        }
        DETAILS_REFRESH_REQUESTED_MESSAGE => {
            emit_host_event(context, NativeHostEvent::RefreshRequested);
            LRESULT(0)
        }
        DETAILS_SETTINGS_REQUESTED_MESSAGE => {
            emit_host_event(context, NativeHostEvent::OpenSettingsRequested);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ANIMATION_TIMER_ID => {
            animate(hwnd, context);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if let Some(webview) = context.taskbar_webview.as_ref() {
                webview.resize(hwnd);
            } else {
                context.renderer.on_device_lost();
            }
            // SAFETY: hwnd 有效；令下一次 WM_PAINT 用新 DPI 和 client size 延迟创建目标。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            };
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Some(webview) = context.taskbar_webview.as_ref() {
                webview.resize(hwnd);
            } else {
                context.renderer.on_device_lost();
            }
            // SAFETY: hwnd 有效；令下一次 WM_PAINT 用新 DPI 和 client size 延迟创建目标。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            };
            LRESULT(0)
        }
        EXIT_MESSAGE => {
            // SAFETY: 当前线程销毁自己的 HWND；WM_DESTROY 负责释放 Rust 上下文并结束消息循环。
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: 未处理消息交给系统默认窗口过程。
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn process_commands(hwnd: HWND, context: &mut WindowContext) {
    while let Ok(command) = context.commands.try_recv() {
        match command {
            NativeHostCommand::Show => {
                context.visible = true;
                // SAFETY: hwnd 属于当前 UI 线程；SW_SHOWNOACTIVATE 不抢占前台焦点。
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                };
                update_timer(hwnd, context);
                // SAFETY: 显示后要求一次完整绘制。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                };
            }
            NativeHostCommand::Hide => {
                context.visible = false;
                stop_timer(hwnd, context);
                // SAFETY: 隐藏仅影响本进程独立窗口。
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                };
            }
            NativeHostCommand::Relocate(rect) => {
                if rect.is_valid() {
                    relocate_in_taskbar(hwnd, context, context.taskbar_parent, rect);
                }
            }
            NativeHostCommand::AttachToTaskbar { parent, rect } => {
                if !rect.is_valid() {
                    tracing::warn!(event = "taskbar_reattach_rejected", "重新挂接的任务栏矩形无效");
                } else {
                    match hwnd_from_raw(parent.hwnd) {
                        Ok(parent_hwnd) => match attach_window(hwnd, parent_hwnd) {
                            Ok(()) => {
                                // Explorer 重启、任务栏方向变化或目标显示器切换都会使旧
                                // popup 的锚点失效。先关闭旧浮层，下次交互再按新任务栏创建，
                                // 避免主体已到副屏而详情卡仍留在原显示器。
                                close_details_card(context);
                                close_token_strip(context);
                                context.taskbar_parent = parent;
                                relocate_in_taskbar(hwnd, context, parent, rect);
                            }
                            Err(error) => tracing::warn!(
                                event = "taskbar_reattach_failed",
                                error = %error,
                                "无法重新挂接到 Explorer 任务栏"
                            ),
                        },
                        Err(error) => tracing::warn!(
                            event = "taskbar_parent_invalid",
                            error = %error,
                            "Explorer 任务栏句柄无效"
                        ),
                    }
                }
            }
            NativeHostCommand::UpdateModel(model) => {
                context.runtime.update(model, tick_count_ms());
                update_timer(hwnd, context);
                if context.taskbar_webview.is_none() {
                    // SAFETY: 额度模型变动影响圆环，必须完整失效。
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    };
                }
            }
            NativeHostCommand::UpdateWebTaskbarSnapshot(snapshot_json) => {
                if let Some(webview) = context.taskbar_webview.as_ref() {
                    webview.post_snapshot(&snapshot_json);
                }
            }
            NativeHostCommand::UpdateWebTokenStripSnapshot(snapshot_json) => {
                context.token_strip_snapshot = Some(snapshot_json);
                if let Some(strip_window) =
                    context.token_strip_window.filter(|window| unsafe { IsWindow(Some(*window)) }.as_bool())
                {
                    let popup_context =
                        unsafe { GetWindowLongPtrW(strip_window, GWLP_USERDATA) as *mut DetailsContext };
                    if !popup_context.is_null() {
                        unsafe { (*popup_context).token_strip_snapshot = context.token_strip_snapshot.clone() };
                    }
                    post_token_strip_snapshot(strip_window, context.token_strip_snapshot.as_deref());
                }
            }
            NativeHostCommand::UpdateDetails(details) => {
                context.details = *details;
                context.details_web_snapshot = Some(details_web_snapshot(&context.details).into_boxed_str());
                // 详情快照同时驱动任务栏胶囊内的文字。此前只重绘了已打开的
                // 弹窗，主窗口会一直保留初始化时的“未知状态”等旧占位内容。
                // 这里必须让任务栏本体也失效，下一次 WM_PAINT 才会读取新快照。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                if let Some(details_window) =
                    context.details_window.filter(|window| unsafe { IsWindow(Some(*window)) }.as_bool())
                {
                    // 更新已打开卡片时直接替换其不可变详情快照并触发重绘。
                    let popup_context =
                        unsafe { GetWindowLongPtrW(details_window, GWLP_USERDATA) as *mut DetailsContext };
                    if !popup_context.is_null() {
                        unsafe { (*popup_context).details = context.details.clone() };
                        post_details_snapshot(details_window, context.details_web_snapshot.as_deref());
                        unsafe {
                            let _ = InvalidateRect(Some(details_window), None, false);
                        }
                    }
                }
                if let Some(strip_window) =
                    context.token_strip_window.filter(|window| unsafe { IsWindow(Some(*window)) }.as_bool())
                {
                    let popup_context =
                        unsafe { GetWindowLongPtrW(strip_window, GWLP_USERDATA) as *mut DetailsContext };
                    if !popup_context.is_null() {
                        unsafe { (*popup_context).details = context.details.clone() };
                        unsafe {
                            let _ = InvalidateRect(Some(strip_window), None, false);
                        }
                    }
                }
            }
            NativeHostCommand::ShowDetails => {
                tracing::info!(event = "details_show_command_received", "已收到详情卡显示命令");
                close_token_strip(context);
                if !context.details_window.is_some_and(|window| unsafe { IsWindow(Some(window)) }.as_bool()) {
                    toggle_details_card(hwnd, context);
                }
            }
            NativeHostCommand::ShowTokenStrip => {
                // 详情卡是用户主动打开的长驻交互面；新 Token 事件属于短暂反馈，
                // 不能为了显示它而销毁详情卡。详情打开期间直接丢弃这一轮快览，
                // 避免详情关闭后又补弹一条已经过期的消费。
                let details_open =
                    context.details_window.is_some_and(|window| unsafe { IsWindow(Some(window)) }.as_bool());
                if should_suppress_token_strip(details_open) {
                    close_token_strip(context);
                    context.token_strip_snapshot = None;
                    tracing::debug!(event = "token_strip_suppressed_by_details", "详情卡打开期间已抑制本次消耗浮窗");
                } else {
                    show_token_strip(hwnd, context);
                }
            }
            NativeHostCommand::RestoreTrayIcon => {
                remove_tray_icon(hwnd, context);
                context.tray_icon = add_tray_icon(hwnd);
                context.tray_icon_registered = context.tray_icon.is_some();
            }
            NativeHostCommand::ShowNotification(notification) => {
                show_tray_notification(hwnd, context, &notification);
            }
            NativeHostCommand::RequestExit => {
                // SAFETY: 延后到当前命令栈返回后再销毁，避免同步 WM_DESTROY 释放仍被借用的上下文。
                let _ = unsafe { PostMessageW(Some(hwnd), EXIT_MESSAGE, WPARAM(0), LPARAM(0)) };
                break;
            }
        }
    }
}

const fn should_suppress_token_strip(details_open: bool) -> bool {
    details_open
}

/// 注册一个始终可达的 Windows 通知区域图标。
///
/// 使用系统应用图标可避免引入额外位图资源；正式品牌图标可以后续替换而不改变
/// 菜单和运行时事件协议。
fn add_tray_icon(hwnd: HWND) -> Option<HICON> {
    let Ok(icon) = create_brand_tray_icon() else {
        tracing::warn!(event = "tray_icon_create_failed", "无法创建 Codex Taskbar 托盘图标");
        return None;
    };
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide_text(&mut data.szTip, "Codex Taskbar");
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
    if !added {
        tracing::warn!(event = "tray_icon_add_failed", "无法注册托盘图标");
        let _ = unsafe { DestroyIcon(icon) };
        return None;
    }
    Some(icon)
}

/// 生成小尺寸仍可辨识的品牌图标：深蓝圆角底、双额度环和青绿状态点。
/// 直接生成 BGRA 像素可避免外部 ico 资源丢失，也不引入图片解码器。
pub(super) fn create_brand_tray_icon() -> Result<HICON, Error> {
    const SIZE: usize = 32;
    let pixels = brand_tray_icon_pixels();
    // AND mask 每行必须按 32 bit 对齐；全 0 表示有效区域由 32-bit alpha 决定。
    let and_mask = [0_u8; SIZE * SIZE / 8];
    unsafe { CreateIcon(None, SIZE as i32, SIZE as i32, 1, 32, and_mask.as_ptr(), pixels.as_ptr()) }
}

fn brand_tray_icon_pixels() -> Vec<u8> {
    const SIZE: usize = 32;
    let mut pixels = vec![0_u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let rounded_inside = rounded_rect_inside(fx, fy, 2.0, 2.0, 30.0, 30.0, 7.0);
            if rounded_inside {
                put_bgra(&mut pixels, x, y, [42, 27, 17, 255]);
            }

            let dx = fx - 15.0;
            let dy = fy - 15.0;
            let distance = (dx * dx + dy * dy).sqrt();
            if (9.2..=11.6).contains(&distance) {
                put_bgra(&mut pixels, x, y, [238, 181, 59, 255]);
            } else if (5.3..=7.2).contains(&distance) {
                put_bgra(&mut pixels, x, y, [219, 126, 70, 255]);
            }

            let dot_dx = fx - 24.0;
            let dot_dy = fy - 24.0;
            let dot_distance = (dot_dx * dot_dx + dot_dy * dot_dy).sqrt();
            if dot_distance <= 4.1 {
                let alpha = if dot_distance <= 2.7 { 255 } else { 190 };
                put_bgra(&mut pixels, x, y, [157, 238, 73, alpha]);
            }
        }
    }
    pixels
}

fn rounded_rect_inside(x: f32, y: f32, left: f32, top: f32, right: f32, bottom: f32, radius: f32) -> bool {
    let closest_x = x.clamp(left + radius, right - radius);
    let closest_y = y.clamp(top + radius, bottom - radius);
    let dx = x - closest_x;
    let dy = y - closest_y;
    dx * dx + dy * dy <= radius * radius
}

fn put_bgra(pixels: &mut [u8], x: usize, y: usize, color: [u8; 4]) {
    // CreateIcon 的 XOR 位图按自底向上扫描。
    let row = 31 - y;
    let offset = (row * 32 + x) * 4;
    pixels[offset..offset + 4].copy_from_slice(&color);
}

/// 删除本进程注册的通知区域图标；重复调用是安全的。
fn remove_tray_icon(hwnd: HWND, context: &mut WindowContext) {
    if !context.tray_icon_registered {
        return;
    }
    let data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    context.tray_icon_registered = false;
    if let Some(icon) = context.tray_icon.take() {
        let _ = unsafe { DestroyIcon(icon) };
    }
}

/// 使用通知区域气泡反馈不会抢占前台窗口；正文由应用层先脱敏。
fn show_tray_notification(hwnd: HWND, context: &WindowContext, notification: &NativeNotification) {
    if !context.tray_icon_registered {
        tracing::warn!(event = "tray_notification_skipped", "托盘图标尚未注册，跳过通知");
        return;
    }
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: match notification.kind {
            NativeNotificationKind::Info => NIIF_INFO,
            NativeNotificationKind::Error => NIIF_ERROR,
        },
        ..Default::default()
    };
    copy_wide_text(&mut data.szInfoTitle, &notification.title);
    copy_wide_text(&mut data.szInfo, &notification.message);
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        tracing::warn!(event = "tray_notification_failed", "无法显示托盘通知");
    }
}

/// 把 Rust 字符串安全复制到 Win32 固定长度 UTF-16 字段，并保留末尾 NUL。
fn copy_wide_text<const N: usize>(target: &mut [u16; N], text: &str) {
    target.fill(0);
    for (slot, value) in target.iter_mut().take(N.saturating_sub(1)).zip(text.encode_utf16()) {
        *slot = value;
    }
}

/// 在鼠标当前位置显示轻量原生菜单。菜单命令通过 `WM_COMMAND` 返回同一 UI 线程。
fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        tracing::warn!(event = "tray_menu_create_failed", "无法创建托盘菜单");
        return;
    };
    let result = (|| -> Result<(), Error> {
        unsafe {
            AppendMenuW(menu, MF_STRING, MENU_SHOW_DETAILS, w!("显示详情"))?;
            AppendMenuW(menu, MF_STRING, MENU_OPEN_SETTINGS, w!("设置…"))?;
            AppendMenuW(menu, MF_STRING, MENU_RELOAD_SETTINGS, w!("重新加载设置"))?;
            AppendMenuW(menu, MF_STRING, MENU_OPEN_CONFIG_DIR, w!("打开配置目录"))?;
            AppendMenuW(menu, MF_STRING, MENU_OPEN_LOG_DIR, w!("打开日志目录"))?;
            AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
            AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("退出"))?;
        }
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor)? };
        // Win32 文档要求先把拥有者切到前台，否则菜单点击外部时可能无法关闭。
        unsafe {
            let _ = SetForegroundWindow(hwnd);
            // 嵌入 Explorer 的子窗口不一定能稳定收到异步 WM_COMMAND。
            // TPM_RETURNCMD 让这里直接取回用户选择，再交给同一个映射函数。
            let command = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, cursor.x, cursor.y, None, hwnd, None);
            if command.0 > 0 {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext;
                if !context.is_null() {
                    handle_menu_command(&mut *context, command.0 as usize);
                }
            }
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(event = "tray_menu_open_failed", error = %error, "托盘菜单打开失败");
    }
    let _ = unsafe { DestroyMenu(menu) };
}

/// 将菜单命令转换为不携带平台句柄的应用事件。
fn handle_menu_command(context: &mut WindowContext, command: usize) {
    let Some(event) = menu_command_event(command) else {
        tracing::debug!(event = "tray_menu_unknown_command", command, "忽略未知托盘菜单命令");
        return;
    };
    tracing::info!(event = "tray_menu_command_selected", command, action = ?event, "用户选择了托盘菜单命令");
    emit_host_event(context, event);
}

fn menu_command_event(command: usize) -> Option<NativeHostEvent> {
    match command {
        MENU_SHOW_DETAILS => NativeHostEvent::ShowDetailsRequested,
        MENU_OPEN_SETTINGS => NativeHostEvent::OpenSettingsRequested,
        MENU_EDIT_SETTINGS => NativeHostEvent::EditSettingsRequested,
        MENU_RELOAD_SETTINGS => NativeHostEvent::ReloadSettingsRequested,
        MENU_OPEN_CONFIG_DIR => NativeHostEvent::OpenConfigDirectoryRequested,
        MENU_OPEN_LOG_DIR => NativeHostEvent::OpenLogDirectoryRequested,
        MENU_EXIT => NativeHostEvent::ExitRequested,
        _ => return None,
    }
    .into()
}

fn emit_host_event(context: &WindowContext, event: NativeHostEvent) {
    if context.events.send(event.clone()).is_err() {
        tracing::debug!(event = "native_host_event_dropped", ?event, "应用运行时已停止接收 UI 操作");
    }
}

struct DetailsContext {
    owner: HWND,
    renderer: Direct2dRenderer,
    /// Token 快览优先使用 WebView2；详情卡暂保留原生渲染直到它自己的迁移阶段。
    webview: Option<TaskbarWebView>,
    /// 快览自身持有的最后一份脱敏快照，用于 WebView2 导航完成后可靠重投。
    token_strip_snapshot: Option<Box<str>>,
    details: NativeHostDetails,
    kind: PopupKind,
    hovered_trend_index: Option<usize>,
    hovered_action: Option<DetailsAction>,
    tracking_mouse: bool,
    pointer_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupKind {
    DetailsCard,
    TokenStrip,
}

fn toggle_details_card(owner: HWND, context: &mut WindowContext) {
    if let Some(window) = context.details_window.take() {
        if unsafe { IsWindow(Some(window)) }.as_bool() {
            let _ = unsafe { DestroyWindow(window) };
            return;
        }
    }
    match create_popup(owner, context.taskbar_parent, context.details.clone(), PopupKind::DetailsCard) {
        Ok(window) => {
            context.details_window = Some(window);
            tracing::info!(event = "details_card_opened", "详情卡片窗口已创建");
            post_details_snapshot(window, context.details_web_snapshot.as_deref());
        }
        Err(error) => tracing::warn!(event = "details_card_open_failed", error = %error, "详情卡片打开失败"),
    }
}

fn close_details_card(context: &mut WindowContext) {
    if let Some(window) = context.details_window.take() {
        if unsafe { IsWindow(Some(window)) }.as_bool() {
            let _ = unsafe { DestroyWindow(window) };
        }
    }
}

fn show_token_strip(owner: HWND, context: &mut WindowContext) {
    if let Some(window) = context.token_strip_window.filter(|window| unsafe { IsWindow(Some(*window)) }.as_bool()) {
        // “ShowTokenStrip” 同时承担刷新语义，供 runtime 在新 turn/token 增量
        // 到达时重复调用；不重建 HWND，因而不会抢焦点或闪烁。
        reset_token_strip_timer(window);
        unsafe {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(window), None, false);
        }
        post_token_strip_snapshot(window, context.token_strip_snapshot.as_deref());
        return;
    }
    match create_popup(owner, context.taskbar_parent, context.details.clone(), PopupKind::TokenStrip) {
        Ok(window) => {
            context.token_strip_window = Some(window);
            let popup_context = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut DetailsContext };
            if !popup_context.is_null() {
                unsafe { (*popup_context).token_strip_snapshot = context.token_strip_snapshot.clone() };
            }
            post_token_strip_snapshot(window, context.token_strip_snapshot.as_deref());
        }
        // 失败意味着用户会完全看不到“本次消耗”反馈。仅记录操作系统错误类别，
        // 不携带账户、额度、线程或 Token 值；Warn 让默认 Info 日志也能定位它。
        Err(error) => tracing::warn!(event = "token_strip_open_failed", error = %error, "自动消费浮窗创建失败"),
    }
}

fn post_token_strip_snapshot(window: HWND, snapshot: Option<&str>) {
    let Some(snapshot) = snapshot else { return };
    let context = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut DetailsContext };
    if !context.is_null() {
        if let Some(webview) = unsafe { (*context).webview.as_ref() } {
            webview.post_snapshot(snapshot);
        }
    }
}

/// 向已打开详情卡补发最近的受限展示快照。失败仅意味着 WebView2 正在导航；
/// 后续 `UpdateDetails` 会重新投递，不能影响原生回退卡片。
fn post_details_snapshot(window: HWND, snapshot: Option<&str>) {
    let Some(snapshot) = snapshot else { return };
    let context = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut DetailsContext };
    if !context.is_null() {
        if let Some(webview) = unsafe { (*context).webview.as_ref() } {
            webview.post_snapshot(snapshot);
        }
    }
}

/// 从平台无关的语义详情模型投影出浏览器可以消费的最小 JSON。
///
/// 特别注意：这里从不传递原始 app-server/SQLite 结构、线程 ID、提示词、
/// 凭据或绝对路径；数组中的文字均已由应用层完成展示格式化。
fn details_web_snapshot(details: &NativeHostDetails) -> String {
    crate::web_snapshot::details_web_snapshot(details)
}
fn close_token_strip(context: &mut WindowContext) {
    context.token_strip_snapshot = None;
    if let Some(window) = context.token_strip_window.take() {
        if unsafe { IsWindow(Some(window)) }.as_bool() {
            let _ = unsafe { DestroyWindow(window) };
        }
    }
}

fn create_popup(
    owner: HWND,
    taskbar: TaskbarParent,
    details: NativeHostDetails,
    kind: PopupKind,
) -> Result<HWND, Error> {
    // Explorer 的任务栏宿主在混合 DPI 环境中有时会报告系统 DPI（例如 96），
    // 而即将创建的独立 popup 实际会落在副屏的 150%/200% DPI 上。先用 owner
    // 做保守的临时布局，创建 HWND 后再以 popup 自己的实际 DPI 重算一次；否则
    // WebView2 的 CSS 视口会被缩成一半，详情卡右侧与趋势区都会被裁切。
    let owner_dpi = unsafe { GetDpiForWindow(owner) }.max(96) as f32;
    let mut scale = owner_dpi / 96.0;
    let (width_dip, height_dip, gap_dip) = match kind {
        // 详情卡使用宽版画布，为额度、今日构成、余额与重置券保留可读空间；
        // Token 快览与任务栏等宽，横向容纳六项“本次”指标和向下指针。
        // 详情卡保留趋势与账户汇总，不再通过隐藏下半区来强塞进 480 DIP。
        // 趋势区增高后同步增加 28 DIP，避免底部“账户与本机记录”被裁切。
        // 宽度保持不变；横向空间由 WebView 内部重新分配给左侧账户栏。
        PopupKind::DetailsCard => (900.0, 668.0, 8.0),
        PopupKind::TokenStrip => (620.0, 112.0, 3.0),
    };
    let desired_width = (width_dip * scale).round() as i32;
    let desired_height = (height_dip * scale).round() as i32;
    let (mut x, mut y, mut width, mut height) =
        popup_position(owner, taskbar, desired_width, desired_height, scale, gap_dip)?;
    let context = Box::new(DetailsContext {
        owner,
        renderer: Direct2dRenderer::new().map_err(|error| with_stage(error, "创建详情卡片渲染器"))?,
        webview: None,
        token_strip_snapshot: None,
        details,
        kind,
        hovered_trend_index: None,
        hovered_action: None,
        tracking_mouse: false,
        pointer_down: false,
    });
    let extended_style = match kind {
        PopupKind::DetailsCard => WS_EX_LAYERED | WS_EX_TOOLWINDOW,
        // 自动浮窗不接受 WS_EX_TRANSPARENT；其显示阶段已通过 SW_SHOWNOACTIVATE
        // 与 SWP_NOACTIVATE 保证不会抢焦点。部分 Explorer 环境中同时设置
        // WS_EX_NOACTIVATE 会让独立 layered popup 创建后不触发首次 WM_PAINT，
        // 因而不把这个扩展位用于 token-strip。
        PopupKind::TokenStrip => WS_EX_LAYERED | WS_EX_TOOLWINDOW,
    };
    let hwnd = unsafe {
        CreateWindowExW(
            extended_style,
            DETAILS_CLASS_NAME,
            w!("Codex 使用详情"),
            WS_POPUP,
            x,
            y,
            width,
            height,
            // 不能把 popup 设为已嵌入 Explorer 的任务栏窗口的 owned window。
            // 跨进程 SetParent 会把 owner 降到系统 DPI context，继承后 WebView2
            // 只能得到一半宽度的 CSS viewport。生命周期仍由 `DetailsContext.owner`
            // 管理，因此这里保持独立顶层 tool window 即可。
            None,
            None,
            None,
            Some(Box::into_raw(context).cast()),
        )?
    };
    // `GetDpiForWindow` 在 Explorer/任务栏 reparent 后仍可能返回 96；显示器的
    // effective DPI 才是 WebView2 计算 CSS viewport 所依据的物理缩放比例。
    let mut monitor_dpi_x = 0;
    let mut monitor_dpi_y = 0;
    let popup_dpi = unsafe {
        GetDpiForMonitor(
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
            MDT_EFFECTIVE_DPI,
            &mut monitor_dpi_x,
            &mut monitor_dpi_y,
        )
        .map(|()| monitor_dpi_x)
        .unwrap_or_else(|_| GetDpiForWindow(hwnd))
    }
    .max(96) as f32;
    tracing::debug!(
        event = "popup_effective_dpi",
        kind = ?kind,
        owner_dpi,
        monitor_dpi_x,
        popup_dpi,
        "详情/快览浮窗已读取有效 DPI"
    );
    if (popup_dpi - owner_dpi).abs() > f32::EPSILON {
        scale = popup_dpi / 96.0;
        let desired_width = (width_dip * scale).round() as i32;
        let desired_height = (height_dip * scale).round() as i32;
        (x, y, width, height) = popup_position(owner, taskbar, desired_width, desired_height, scale, gap_dip)?;
    }
    if matches!(kind, PopupKind::DetailsCard | PopupKind::TokenStrip) {
        // WebView 的透明圆角外侧会露出旧 Direct2D 回退层；用 HWND region 同步
        // 裁剪，让详情卡与 Token 快览顶部都与 CSS 的玻璃圆角成为一个整体。
        let corner_dip = if kind == PopupKind::DetailsCard { 26.0 } else { 17.0 };
        let corner = (corner_dip * scale).round() as i32;
        let region = unsafe { CreateRoundRectRgn(0, 0, width, height, corner * 2, corner * 2) };
        if !region.is_invalid() {
            let _ = unsafe { SetWindowRgn(hwnd, Some(region), true) };
        }
    }
    if kind == PopupKind::TokenStrip {
        let mut owner_rect = RECT::default();
        unsafe { GetWindowRect(owner, &mut owner_rect)? };
        let owner_center_x = (owner_rect.left + owner_rect.right) as f32 / 2.0;
        let popup_left = x as f32;
        let max_arrow_x = ((width as f32 / scale) - 18.0).max(18.0);
        let arrow_x_dip = ((owner_center_x - popup_left) / scale).clamp(18.0, max_arrow_x);
        let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DetailsContext };
        if !context.is_null() {
            match TaskbarWebView::create_with_document(hwnd, token_strip_web_document(arrow_x_dip)) {
                Ok(webview) => unsafe { (*context).webview = Some(webview) },
                Err(error) => tracing::warn!(
                    event = "token_strip_webview_fallback",
                    error = %error,
                    "Token 快览 WebView2 不可用，已回退原生渲染"
                ),
            }
        }
    }
    if kind == PopupKind::DetailsCard {
        let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DetailsContext };
        if !context.is_null() {
            match TaskbarWebView::create_with_document(hwnd, details_card_web_document(scale)) {
                Ok(webview) => unsafe { (*context).webview = Some(webview) },
                Err(error) => tracing::warn!(
                    event = "details_card_webview_fallback",
                    error = %error,
                    "详情卡 WebView2 不可用，已回退原生渲染"
                ),
            }
        }
    }
    unsafe {
        let show_flags = if kind == PopupKind::TokenStrip { SWP_NOACTIVATE | SWP_SHOWWINDOW } else { SWP_NOACTIVATE };
        SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, show_flags)?;
        // 详情卡仍使用普通显示，以便接收键盘和鼠标；Token 浮窗已由上面的
        // SWP_SHOWWINDOW 明确显示，此调用只保持历史兼容，不依赖它触发首帧。
        let _ = ShowWindow(hwnd, if kind == PopupKind::DetailsCard { SW_SHOW } else { SW_SHOWNOACTIVATE });
        // create_popup 会先以初始 DIP 尺寸创建 HWND，再依据目标副屏的有效 DPI、
        // 可用工作区重算最终尺寸。WebView2 控制器不会自动跟随这一后置 SetWindowPos；
        // 若不显式同步，它会保留较小的旧视口，右侧摘要卡便会被裁切。
        let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DetailsContext;
        if !context.is_null() {
            if let Some(webview) = (*context).webview.as_ref() {
                webview.resize(hwnd);
            }
        }
        if kind == PopupKind::TokenStrip {
            reset_token_strip_timer(hwnd);
        }
        if kind == PopupKind::DetailsCard {
            let _ = SetTimer(Some(hwnd), DETAILS_OUTSIDE_CLICK_TIMER_ID, DETAILS_OUTSIDE_CLICK_POLL_MS, None);
            let _ = SetForegroundWindow(hwnd);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    Ok(hwnd)
}

fn popup_position(
    owner: HWND,
    taskbar: TaskbarParent,
    desired_width: i32,
    desired_height: i32,
    scale: f32,
    gap_dip: f32,
) -> Result<(i32, i32, i32, i32), Error> {
    let mut widget = RECT::default();
    unsafe { GetWindowRect(owner, &mut widget)? };
    let monitor = unsafe { MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return Err(Error::from_thread());
    }
    let gap = (gap_dip * scale).round() as i32;
    let margin = (8.0 * scale).round() as i32;
    let widget = PixelRect { left: widget.left, top: widget.top, right: widget.right, bottom: widget.bottom };
    let work = PixelRect {
        left: info.rcWork.left,
        top: info.rcWork.top,
        right: info.rcWork.right,
        bottom: info.rcWork.bottom,
    };
    let geometry =
        fit_popup_to_work_area(widget, taskbar.screen_rect, work, desired_width, desired_height, gap, margin)
            .ok_or_else(|| Error::new(E_INVALIDARG, "弹窗或显示器工作区几何无效"))?;
    Ok((geometry.x, geometry.y, geometry.width, geometry.height))
}

/// 弹窗最终使用的屏幕物理像素几何。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopupGeometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// 在当前显示器工作区中放置弹窗。
///
/// 先从 `rcWork` 四周扣除 DPI 对应边距，再对期望尺寸做等比缩小；缩放因子
/// 最大为 1，因此空间充足时绝不会意外放大。定位以任务栏方向为准，最后再
/// 夹紧到安全工作区，兼容负坐标副屏和尺寸很小的工作区。
fn fit_popup_to_work_area(
    widget: PixelRect,
    bar: PixelRect,
    work: PixelRect,
    desired_width: i32,
    desired_height: i32,
    gap: i32,
    margin: i32,
) -> Option<PopupGeometry> {
    if !widget.is_valid() || !bar.is_valid() || !work.is_valid() || desired_width <= 0 || desired_height <= 0 {
        return None;
    }

    // 极小工作区仍至少保留一个可绘制像素；正常工作区则完整保留要求的边距。
    let margin = margin.max(0);
    let margin_x = margin.min(work.width().saturating_sub(1) / 2);
    let margin_y = margin.min(work.height().saturating_sub(1) / 2);
    let safe_left = work.left.saturating_add(margin_x);
    let safe_top = work.top.saturating_add(margin_y);
    let safe_right = work.right.saturating_sub(margin_x);
    let safe_bottom = work.bottom.saturating_sub(margin_y);
    let available_width = safe_right.saturating_sub(safe_left);
    let available_height = safe_bottom.saturating_sub(safe_top);
    if available_width <= 0 || available_height <= 0 {
        return None;
    }

    let fit_scale =
        (available_width as f64 / desired_width as f64).min(available_height as f64 / desired_height as f64).min(1.0);
    let width = ((desired_width as f64 * fit_scale).floor() as i32).clamp(1, available_width);
    let height = ((desired_height as f64 * fit_scale).floor() as i32).clamp(1, available_height);
    let gap = gap.max(0);
    let horizontal = bar.width() >= bar.height();
    let (mut x, mut y) = if horizontal {
        let centered = widget.left + (widget.right - widget.left - width) / 2;
        if bar.top >= work.bottom {
            (centered, widget.top.saturating_sub(gap).saturating_sub(height))
        } else {
            (centered, widget.bottom.saturating_add(gap))
        }
    } else {
        let centered = widget.top + (widget.bottom - widget.top - height) / 2;
        if bar.left >= work.right {
            (widget.left.saturating_sub(gap).saturating_sub(width), centered)
        } else {
            (widget.right.saturating_add(gap), centered)
        }
    };
    let max_x = safe_right.saturating_sub(width).max(safe_left);
    let max_y = safe_bottom.saturating_sub(height).max(safe_top);
    x = x.clamp(safe_left, max_x);
    y = y.clamp(safe_top, max_y);
    Some(PopupGeometry { x, y, width, height })
}

unsafe extern "system" fn details_window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DetailsContext };
    if context.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_PAINT => {
            let context = unsafe { &mut *context };
            let mut paint = PAINTSTRUCT::default();
            let _ = unsafe { BeginPaint(hwnd, &mut paint) };
            if context.webview.is_some() {
                // WebView2 负责毛玻璃与背景漂移；不要让旧 Direct2D Token-strip
                // 在透明区域再次绘制深色底。
                unsafe {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(0);
            }
            if !context.renderer.has_device_resources() {
                let mut client = RECT::default();
                if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
                    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
                    let _ = context.renderer.recreate_device_resources(
                        hwnd,
                        client.right.saturating_sub(client.left) as u32,
                        client.bottom.saturating_sub(client.top) as u32,
                        dpi,
                    );
                }
            }
            let draw_result = match context.kind {
                PopupKind::DetailsCard => context.renderer.draw_details_card(
                    hwnd,
                    &context.details,
                    context.hovered_trend_index,
                    context.hovered_action,
                ),
                PopupKind::TokenStrip => context.renderer.draw_token_strip(hwnd, &context.details),
            };
            if draw_result.is_err() {
                context.renderer.on_device_lost();
            }
            unsafe {
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER
            if unsafe { (*context).kind } == PopupKind::TokenStrip && wparam.0 == TOKEN_STRIP_AUTO_HIDE_TIMER_ID =>
        {
            // Timer 只绑定到 token-strip HWND；DestroyWindow 会发出关闭通知，
            // owner 侧随后清除悬浮层句柄。窗口本身使用 NOACTIVATE，不影响前台应用。
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_TIMER
            if unsafe { (*context).kind } == PopupKind::DetailsCard && wparam.0 == DETAILS_OUTSIDE_CLICK_TIMER_ID =>
        {
            let context = unsafe { &mut *context };
            let pointer_down = unsafe {
                GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0
                    || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0
                    || GetAsyncKeyState(VK_MBUTTON.0 as i32) < 0
            };
            if pointer_down && !context.pointer_down {
                let mut cursor = POINT::default();
                let mut bounds = RECT::default();
                if unsafe { GetCursorPos(&mut cursor) }.is_ok()
                    && unsafe { GetWindowRect(hwnd, &mut bounds) }.is_ok()
                    && !point_in_window_rect(cursor, bounds)
                {
                    let _ = unsafe { DestroyWindow(hwnd) };
                    return LRESULT(0);
                }
            }
            context.pointer_down = pointer_down;
            LRESULT(0)
        }
        WM_MOUSEMOVE if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let context = unsafe { &mut *context };
            if !context.tracking_mouse {
                let mut event = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut event) }.is_ok() {
                    context.tracking_mouse = true;
                }
            }
            let mut client = RECT::default();
            if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
                let x = (lparam.0 as u32 & 0xffff) as u16 as i16 as i32;
                let y = ((lparam.0 as u32 >> 16) & 0xffff) as u16 as i16 as i32;
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
                let size =
                    (client.right.saturating_sub(client.left) as u32, client.bottom.saturating_sub(client.top) as u32);
                let hovered_action = details_action_hit_test(size, dpi, (x, y));
                let hovered = details_trend_hit_test(size, dpi, &context.details, (x, y));
                if hovered != context.hovered_trend_index || hovered_action != context.hovered_action {
                    context.hovered_trend_index = hovered;
                    context.hovered_action = hovered_action;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MESSAGE if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let context = unsafe { &mut *context };
            context.tracking_mouse = false;
            if context.hovered_trend_index.take().is_some() || context.hovered_action.take().is_some() {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let context = unsafe { &mut *context };
            // WebView2 已通过受限消息桥处理刷新/设置。若仍用旧 Direct2D 的
            // 坐标命中，会在高 DPI 缩放后把普通卡片点击误判成按钮，进而销毁
            // 再重建详情卡，表现为用户看到的闪动。
            if context.webview.is_some() {
                return LRESULT(0);
            }
            let mut client = RECT::default();
            if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
                let x = (lparam.0 as u32 & 0xffff) as u16 as i16 as i32;
                let y = ((lparam.0 as u32 >> 16) & 0xffff) as u16 as i16 as i32;
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
                let action = details_action_hit_test(
                    (client.right.saturating_sub(client.left) as u32, client.bottom.saturating_sub(client.top) as u32),
                    dpi,
                    (x, y),
                );
                if let Some(message) = details_action_message(action) {
                    // 先把意图投递给 owner，再关闭旧详情卡。owner 的消息循环会
                    // 依次转成 NativeHostEvent，避免刷新或设置窗口与旧 popup 重叠。
                    let _ = unsafe { PostMessageW(Some(context.owner), message, WPARAM(0), LPARAM(0)) };
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
            }
            LRESULT(0)
        }
        DETAILS_REFRESH_REQUESTED_MESSAGE if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let owner = unsafe { (*context).owner };
            let _ = unsafe { PostMessageW(Some(owner), DETAILS_REFRESH_REQUESTED_MESSAGE, WPARAM(0), LPARAM(0)) };
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        DETAILS_SETTINGS_REQUESTED_MESSAGE if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let owner = unsafe { (*context).owner };
            let _ = unsafe { PostMessageW(Some(owner), DETAILS_SETTINGS_REQUESTED_MESSAGE, WPARAM(0), LPARAM(0)) };
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        DETAILS_WEB_READY_MESSAGE if unsafe { (*context).kind } == PopupKind::DetailsCard => {
            let context = unsafe { &mut *context };
            // 网页已完成 DOM 初始化，现从 popup 自己持有的不可变详情快照补发。
            // 快照只包含已格式化 UI 字段，不会把原始账户或 SQLite 数据交给网页。
            let snapshot = details_web_snapshot(&context.details);
            post_details_snapshot(hwnd, Some(&snapshot));
            LRESULT(0)
        }
        TOKEN_STRIP_WEB_READY_MESSAGE if unsafe { (*context).kind } == PopupKind::TokenStrip => {
            let context = unsafe { &mut *context };
            // 快览页面已经安装消息监听器；此时再投递才不会丢失新 Token 的首帧。
            if let Some(webview) = context.webview.as_ref() {
                if let Some(snapshot) = context.token_strip_snapshot.as_deref() {
                    webview.post_snapshot(snapshot);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == 27 => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        // WebView2 的输入焦点会落到子 HWND，单独依赖 WM_KILLFOCUS 会把卡片
        // 内部点击误判为外部点击，因此这里只忽略焦点切换。
        WM_KILLFOCUS if unsafe { (*context).kind } == PopupKind::DetailsCard => LRESULT(0),
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            unsafe { (*context).renderer.on_device_lost() };
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let owner = unsafe { (*context).owner };
            let kind = unsafe { (*context).kind };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(context));
                let message =
                    if kind == PopupKind::DetailsCard { DETAILS_CLOSED_MESSAGE } else { TOKEN_STRIP_CLOSED_MESSAGE };
                let _ = PostMessageW(Some(owner), message, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn reset_token_strip_timer(hwnd: HWND) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), TOKEN_STRIP_AUTO_HIDE_TIMER_ID);
        let _ = SetTimer(Some(hwnd), TOKEN_STRIP_AUTO_HIDE_TIMER_ID, TOKEN_STRIP_AUTO_HIDE_MS, None);
    }
}

fn details_action_message(action: Option<DetailsAction>) -> Option<u32> {
    match action? {
        DetailsAction::Refresh => Some(DETAILS_REFRESH_REQUESTED_MESSAGE),
        DetailsAction::OpenSettings => Some(DETAILS_SETTINGS_REQUESTED_MESSAGE),
    }
}

const fn point_in_window_rect(point: POINT, rect: RECT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

fn relocate_in_taskbar(hwnd: HWND, context: &mut WindowContext, parent: TaskbarParent, rect: PixelRect) {
    let Ok(client_rect) = screen_to_taskbar_client(rect, parent) else { return };
    // SAFETY: client_rect 属于当前 parent 的坐标空间；HWND_TOP 只调整 sibling Z-order。
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            client_rect.left,
            client_rect.top,
            client_rect.width(),
            client_rect.height(),
            SWP_NOACTIVATE,
        )
    };
    if moved.is_ok() {
        if let Some(webview) = context.taskbar_webview.as_ref() {
            webview.resize(hwnd);
        } else {
            context.renderer.on_device_lost();
            // SAFETY: 尺寸或父窗口变化后完整重建 layered surface。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            };
        }
    }
}

fn hwnd_from_raw(raw: isize) -> Result<HWND, Error> {
    if raw == 0 { Err(Error::new(E_INVALIDARG, "任务栏 HWND 为空")) } else { Ok(HWND(raw as *mut c_void)) }
}

fn attach_window(hwnd: HWND, parent: HWND) -> Result<(), Error> {
    // SetParent 不会自动切换 WS_POPUP/WS_CHILD；按 Win32 契约先修改 style。
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let child_style = (style & !(WS_POPUP.0 as isize)) | WS_CHILD.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, child_style);
    }
    // 成功时返回旧父窗口；旧父窗口为桌面时可能是 NULL，windows-rs 会把它包装
    // 成 Err，因此最终以 GetParent 结果作为成功条件。
    let set_parent_result = unsafe { SetParent(hwnd, Some(parent)) };
    if unsafe { GetParent(hwnd) }.is_ok_and(|actual| actual == parent) {
        Ok(())
    } else {
        Err(set_parent_result.err().map_or_else(
            || Error::new(E_INVALIDARG, "无法挂接到 Explorer 任务栏"),
            |error| with_stage(error, "挂接 Explorer 任务栏"),
        ))
    }
}

fn screen_to_taskbar_client(rect: PixelRect, parent: TaskbarParent) -> Result<PixelRect, Error> {
    if !rect.is_valid() || !parent.screen_rect.is_valid() {
        return Err(Error::new(E_INVALIDARG, "任务栏或组件矩形无效"));
    }
    Ok(PixelRect {
        left: rect.left.saturating_sub(parent.screen_rect.left),
        top: rect.top.saturating_sub(parent.screen_rect.top),
        right: rect.right.saturating_sub(parent.screen_rect.left),
        bottom: rect.bottom.saturating_sub(parent.screen_rect.top),
    })
}

/// TrafficMonitor 视觉窗口的最小只读枚举上下文。
///
/// 不保存 HWND，也不读取任何性能文本；窗口可能随时关闭，因此只在创建任务栏
/// 胶囊前使用一次矩形快照。
struct TrafficMonitorWindowScan {
    taskbar: PixelRect,
    rect: Option<PixelRect>,
    matched_window_count: u8,
}

fn avoid_traffic_monitor_overlap(rect: PixelRect, parent: TaskbarParent) -> PixelRect {
    let mut scan = TrafficMonitorWindowScan { taskbar: parent.screen_rect, rect: None, matched_window_count: 0 };
    // SAFETY: lparam 仅在同步 EnumWindows 调用期间指向栈上 scan；回调不会保存它。
    let _ = unsafe {
        EnumWindows(
            Some(enum_traffic_monitor_window),
            LPARAM((&mut scan as *mut TrafficMonitorWindowScan).cast::<c_void>() as isize),
        )
    };
    tracing::info!(
        event = "traffic_monitor_auto_avoid_scan",
        matched_window_count = scan.matched_window_count,
        has_usable_rect = scan.rect.is_some(),
        "已完成 TrafficMonitor 任务栏避让探测"
    );
    let Some(obstacle) = scan.rect else {
        if scan.matched_window_count > 0 {
            tracing::info!(
                event = "traffic_monitor_auto_avoid_no_usable_rect",
                matched_window_count = scan.matched_window_count,
                "检测到 TrafficMonitor，但未取得可用于任务栏避让的窗口范围"
            );
        }
        return rect;
    };
    let adjusted = shift_rect_away_from_obstacle(rect, obstacle, parent.screen_rect, 8);
    if adjusted != rect {
        tracing::info!(event = "traffic_monitor_auto_avoid_applied", "已自动避让 TrafficMonitor 任务栏仪表");
    }
    adjusted
}

/// EnumWindows 回调：只匹配 TrafficMonitor 的随机后缀窗口类，忽略其 tooltip
/// 和输入法辅助窗口。TrafficMonitor 的任务栏模式会在不同 DPI/Explorer 版本下
/// 报告虚拟化的纵坐标，因此只以同一显示器的横向区间作为避让依据；这避免了
/// 正确的左侧仪表因纵坐标换算差异而漏检。
unsafe extern "system" fn enum_traffic_monitor_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut TrafficMonitorWindowScan) };
    if scan.rect.is_some() {
        return BOOL(0);
    }
    let mut class_name = [0_u16; 80];
    let class_length = unsafe { GetClassNameW(hwnd, &mut class_name) };
    if class_length <= 0 {
        return BOOL(1);
    }
    let name = String::from_utf16_lossy(&class_name[..class_length as usize]);
    if !name.starts_with("TrafficMonitor_") {
        return BOOL(1);
    }
    scan.matched_window_count = scan.matched_window_count.saturating_add(1);
    let mut raw = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut raw) }.is_err() {
        return BOOL(1);
    }
    let candidate = PixelRect { left: raw.left, top: raw.top, right: raw.right, bottom: raw.bottom };
    if candidate.is_valid() && candidate.right > scan.taskbar.left && candidate.left < scan.taskbar.right {
        scan.rect = Some(candidate);
        return BOOL(0);
    }
    BOOL(1)
}

/// 在任务栏可用横向范围内把任务栏胶囊移到障碍物左/右，不缩小、不跨屏。
///
/// 左边障碍优先向右移，右边障碍优先向左移；两边均无空间时保留原布局，让用户
/// 仍能通过手动偏移精确控制，而不是静默裁剪组件内容。
fn shift_rect_away_from_obstacle(rect: PixelRect, obstacle: PixelRect, taskbar: PixelRect, gap: i32) -> PixelRect {
    let intersects = rect.left < obstacle.right && rect.right > obstacle.left;
    // TrafficMonitor 在任务栏模式下会把纵坐标虚拟化为自己的逻辑 DPI；横向范围
    // 才是稳定的占位信息，不能再用纵向交集否决一次已确认的同屏窗口匹配。
    if !intersects {
        return rect;
    }
    let width = rect.width();
    let after_left = obstacle.right.saturating_add(gap);
    let before_right = obstacle.left.saturating_sub(gap);
    let can_move_right = after_left.saturating_add(width) <= taskbar.right;
    let can_move_left = before_right.saturating_sub(width) >= taskbar.left;

    // 左半区的 TrafficMonitor 通常是左侧性能文字，优先把胶囊放到它右边；
    // 右半区则优先放到它左边，避免把右锚组件推向系统通知区域。
    let obstacle_is_on_left_half =
        obstacle.left.saturating_add(obstacle.right) <= taskbar.left.saturating_add(taskbar.right);
    if obstacle_is_on_left_half {
        if can_move_right {
            return PixelRect { left: after_left, top: rect.top, right: after_left + width, bottom: rect.bottom };
        }
        if can_move_left {
            return PixelRect { left: before_right - width, top: rect.top, right: before_right, bottom: rect.bottom };
        }
    } else {
        if can_move_left {
            return PixelRect { left: before_right - width, top: rect.top, right: before_right, bottom: rect.bottom };
        }
        if can_move_right {
            return PixelRect { left: after_left, top: rect.top, right: after_left + width, bottom: rect.bottom };
        }
    }
    rect
}

fn paint(hwnd: HWND, context: &mut WindowContext) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: WM_PAINT 期间必须成对调用；PAINTSTRUCT 是有效写入缓冲区。
    let _hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    let result = paint_inner(hwnd, context, paint.rcPaint);
    if result.is_err() {
        context.renderer.on_device_lost();
    }
    // SAFETY: 与上面的 BeginPaint 使用同一 hwnd 和 PAINTSTRUCT 成对结束。
    unsafe {
        let _ = EndPaint(hwnd, &paint);
    };
}

fn paint_inner(hwnd: HWND, context: &mut WindowContext, dirty_px: RECT) -> Result<(), Error> {
    if !context.renderer.has_device_resources() {
        let mut client = RECT::default();
        // SAFETY: client 是有效可写矩形，hwnd 属于当前 UI 线程。
        unsafe { GetClientRect(hwnd, &mut client)? };
        // SAFETY: 只读取本窗口当前 DPI；0 作为异常值回退到 96。
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        context.renderer.recreate_device_resources(
            hwnd,
            client.right.saturating_sub(client.left) as u32,
            client.bottom.saturating_sub(client.top) as u32,
            dpi as f32,
        )?;
    }
    context.renderer.draw(
        hwnd,
        &context.runtime.frame(tick_count_ms()),
        &context.details,
        Some(dip_rect_from_pixels(hwnd, dirty_px)),
    )
}

fn animate(hwnd: HWND, context: &mut WindowContext) {
    let frame = context.runtime.frame(tick_count_ms());
    if frame.animation.next_frame_at_ms.is_some() && context.visible {
        // 流式额度的相位、内部流光、前沿高光和粒子都在整枚胶囊中变化。此前只
        // 失效了已废弃状态灯的极小区域，因而绝大部分动画没有进入 WM_PAINT，表现
        // 为断续跳动。向外扩 7 DIP 以覆盖前沿柔光与微粒，文字仍由同一帧重绘。
        invalidate_dip_rect(hwnd, frame.fluid.bounds.inset(-7.0));
    } else {
        stop_timer(hwnd, context);
    }
}

fn update_timer(hwnd: HWND, context: &mut WindowContext) {
    if context.taskbar_webview.is_some() {
        // WebGL 通过 requestAnimationFrame 使用 Chromium 的显示节奏；不再让旧
        // Direct2D 60 FPS 定时器重复触发 WM_PAINT，以免两套渲染器争夺同一 HWND。
        stop_timer(hwnd, context);
        return;
    }
    let animate = context.visible && context.runtime.frame(tick_count_ms()).animation.next_frame_at_ms.is_some();
    if animate && !context.timer_running {
        // SAFETY: HWND timer 只向当前 UI 线程发送 WM_TIMER；正常模式维持约 60 FPS
        // 的流场连续性。降低动画设置仍会在上层显式停表。
        if unsafe { SetTimer(Some(hwnd), ANIMATION_TIMER_ID, FRAME_MS, None) } != 0 {
            context.timer_running = true;
        }
    } else if !animate {
        stop_timer(hwnd, context);
    }
}

fn stop_timer(hwnd: HWND, context: &mut WindowContext) {
    if context.timer_running {
        // SAFETY: 仅取消由此窗口创建的固定 timer id。
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), ANIMATION_TIMER_ID) };
        context.timer_running = false;
    }
}

fn invalidate_dip_rect(hwnd: HWND, bounds: DipRect) {
    // SAFETY: 只读取本窗口 DPI，按 Direct2D 的 96-DPI DIP 规则转换为物理像素。
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    let dirty = RECT {
        left: (bounds.left * scale).floor() as i32,
        top: (bounds.top * scale).floor() as i32,
        right: (bounds.right * scale).ceil() as i32,
        bottom: (bounds.bottom * scale).ceil() as i32,
    };
    // SAFETY: 仅失效调用方指定的物理像素范围，不清背景。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), Some(&dirty), false);
    };
}

fn dip_rect_from_pixels(hwnd: HWND, pixels: RECT) -> DipRect {
    // SAFETY: 只读取本窗口 DPI，按 Direct2D 的 96-DPI DIP 规则反向转换系统脏区。
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    DipRect {
        left: pixels.left as f32 / scale,
        top: pixels.top as f32 / scale,
        right: pixels.right as f32 / scale,
        bottom: pixels.bottom as f32 / scale,
    }
}

fn tick_count_ms() -> u64 {
    // SAFETY: GetTickCount64 无参数且只读取系统单调计数器。
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskbar_webview_receives_the_configured_physical_width() {
        assert!(taskbar_web_document(283).contains("window.__CodexTaskbarPhysicalWidth=283"));
        assert!(taskbar_web_document(10).contains("window.__CodexTaskbarPhysicalWidth=200"));
        assert!(taskbar_web_document(9_999).contains("window.__CodexTaskbarPhysicalWidth=620"));
    }

    #[test]
    fn screen_rect_is_converted_to_secondary_taskbar_client_coordinates() {
        let parent =
            TaskbarParent { hwnd: 42, screen_rect: PixelRect { left: 2560, top: 1392, right: 5120, bottom: 1440 } };
        let screen = PixelRect { left: 4300, top: 1392, right: 4620, bottom: 1440 };
        assert_eq!(
            screen_to_taskbar_client(screen, parent).unwrap(),
            PixelRect { left: 1740, top: 0, right: 2060, bottom: 48 }
        );
    }

    #[test]
    fn invalid_parent_geometry_is_rejected() {
        let parent = TaskbarParent { hwnd: 42, screen_rect: PixelRect { left: 0, top: 0, right: 0, bottom: 0 } };
        assert!(screen_to_taskbar_client(PixelRect { left: 0, top: 0, right: 10, bottom: 10 }, parent).is_err());
    }

    #[test]
    fn traffic_monitor_on_left_moves_overlapping_widget_to_the_right() {
        let taskbar = PixelRect { left: 0, top: 1_528, right: 2_560, bottom: 1_600 };
        let widget = PixelRect { left: 8, top: 1_528, right: 448, bottom: 1_600 };
        let traffic = PixelRect { left: 0, top: 1_528, right: 520, bottom: 1_600 };

        assert_eq!(
            shift_rect_away_from_obstacle(widget, traffic, taskbar, 8),
            PixelRect { left: 528, top: 1_528, right: 968, bottom: 1_600 }
        );
    }

    #[test]
    fn traffic_monitor_on_right_moves_overlapping_widget_to_the_left() {
        let taskbar = PixelRect { left: 0, top: 1_528, right: 2_560, bottom: 1_600 };
        let widget = PixelRect { left: 1_700, top: 1_528, right: 2_140, bottom: 1_600 };
        let traffic = PixelRect { left: 1_620, top: 1_528, right: 2_000, bottom: 1_600 };

        assert_eq!(
            shift_rect_away_from_obstacle(widget, traffic, taskbar, 8),
            PixelRect { left: 1_172, top: 1_528, right: 1_612, bottom: 1_600 }
        );
    }

    #[test]
    fn non_overlapping_or_out_of_space_traffic_monitor_keeps_user_layout() {
        let taskbar = PixelRect { left: 0, top: 1_528, right: 600, bottom: 1_600 };
        let widget = PixelRect { left: 8, top: 1_528, right: 448, bottom: 1_600 };
        let no_overlap = PixelRect { left: 480, top: 1_528, right: 600, bottom: 1_600 };
        let full_overlap = PixelRect { left: 0, top: 1_528, right: 600, bottom: 1_600 };

        assert_eq!(shift_rect_away_from_obstacle(widget, no_overlap, taskbar, 8), widget);
        assert_eq!(shift_rect_away_from_obstacle(widget, full_overlap, taskbar, 8), widget);
    }

    #[test]
    fn tray_menu_commands_map_to_platform_neutral_events() {
        assert_eq!(menu_command_event(MENU_SHOW_DETAILS), Some(NativeHostEvent::ShowDetailsRequested));
        assert_eq!(menu_command_event(MENU_OPEN_SETTINGS), Some(NativeHostEvent::OpenSettingsRequested));
        assert_eq!(menu_command_event(MENU_RELOAD_SETTINGS), Some(NativeHostEvent::ReloadSettingsRequested));
        assert_eq!(menu_command_event(MENU_OPEN_CONFIG_DIR), Some(NativeHostEvent::OpenConfigDirectoryRequested));
        assert_eq!(menu_command_event(MENU_OPEN_LOG_DIR), Some(NativeHostEvent::OpenLogDirectoryRequested));
        assert_eq!(menu_command_event(MENU_EXIT), Some(NativeHostEvent::ExitRequested));
        assert_eq!(menu_command_event(999_999), None);
    }

    #[test]
    fn details_actions_map_to_owner_messages_before_popup_close() {
        assert_eq!(details_action_message(Some(DetailsAction::Refresh)), Some(DETAILS_REFRESH_REQUESTED_MESSAGE));
        assert_eq!(details_action_message(Some(DetailsAction::OpenSettings)), Some(DETAILS_SETTINGS_REQUESTED_MESSAGE));
        assert_eq!(details_action_message(None), None);
    }

    #[test]
    fn details_outside_click_uses_actual_popup_bounds() {
        let rect = RECT { left: -900, top: 100, right: 0, bottom: 768 };
        assert!(point_in_window_rect(POINT { x: -450, y: 400 }, rect));
        assert!(!point_in_window_rect(POINT { x: 20, y: 400 }, rect));
        assert!(!point_in_window_rect(POINT { x: -450, y: 768 }, rect));
    }

    #[test]
    fn automatic_token_strip_never_replaces_an_open_details_card() {
        assert!(should_suppress_token_strip(true));
        assert!(!should_suppress_token_strip(false));
    }

    #[test]
    fn taskbar_webview_right_click_maps_back_to_the_native_menu() {
        assert_eq!(web_action_message(Some("show-menu")), Some(WM_CONTEXTMENU));
        assert_eq!(web_action_message(Some("show-details")), Some(WM_LBUTTONUP));
        assert_eq!(web_action_message(Some("token-strip-ready")), Some(TOKEN_STRIP_WEB_READY_MESSAGE));
        assert_eq!(web_action_message(Some("untrusted-command")), None);
    }

    #[test]
    fn generated_tray_icon_contains_transparency_brand_rings_and_status_green() {
        let pixels = brand_tray_icon_pixels();
        assert_eq!(pixels.len(), 32 * 32 * 4);
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(pixels.chunks_exact(4).any(|pixel| pixel == [238, 181, 59, 255]));
        assert!(pixels.chunks_exact(4).any(|pixel| pixel == [219, 126, 70, 255]));
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[1] == 238 && pixel[2] == 73));
    }

    #[test]
    fn tray_tooltip_copy_is_nul_terminated_and_truncated() {
        let mut short = [0_u16; 8];
        copy_wide_text(&mut short, "Codex");
        assert_eq!(&short[..6], &[67, 111, 100, 101, 120, 0]);

        let mut truncated = [0_u16; 4];
        copy_wide_text(&mut truncated, "Taskbar");
        assert_eq!(truncated, [84, 97, 115, 0]);
    }

    #[test]
    fn popup_is_placed_above_bottom_taskbar() {
        let geometry = fit_popup_to_work_area(
            PixelRect { left: 1_200, top: 1_040, right: 1_520, bottom: 1_080 },
            PixelRect { left: 0, top: 1_040, right: 1_920, bottom: 1_080 },
            PixelRect { left: 0, top: 0, right: 1_920, bottom: 1_040 },
            880,
            590,
            8,
            8,
        )
        .unwrap();

        assert_eq!(geometry, PopupGeometry { x: 920, y: 442, width: 880, height: 590 });
    }

    #[test]
    fn popup_is_placed_below_top_taskbar() {
        let geometry = fit_popup_to_work_area(
            PixelRect { left: 400, top: 0, right: 720, bottom: 40 },
            PixelRect { left: 0, top: 0, right: 1_920, bottom: 40 },
            PixelRect { left: 0, top: 40, right: 1_920, bottom: 1_080 },
            880,
            590,
            8,
            8,
        )
        .unwrap();

        assert_eq!(geometry, PopupGeometry { x: 120, y: 48, width: 880, height: 590 });
    }

    #[test]
    fn popup_is_placed_beside_left_and_right_taskbars() {
        let left = fit_popup_to_work_area(
            PixelRect { left: 0, top: 300, right: 48, bottom: 620 },
            PixelRect { left: 0, top: 0, right: 48, bottom: 1_080 },
            PixelRect { left: 48, top: 0, right: 1_920, bottom: 1_080 },
            880,
            590,
            8,
            8,
        )
        .unwrap();
        let right = fit_popup_to_work_area(
            PixelRect { left: 1_872, top: 300, right: 1_920, bottom: 620 },
            PixelRect { left: 1_872, top: 0, right: 1_920, bottom: 1_080 },
            PixelRect { left: 0, top: 0, right: 1_872, bottom: 1_080 },
            880,
            590,
            8,
            8,
        )
        .unwrap();

        assert_eq!(left, PopupGeometry { x: 56, y: 165, width: 880, height: 590 });
        assert_eq!(right, PopupGeometry { x: 984, y: 165, width: 880, height: 590 });
    }

    #[test]
    fn popup_supports_negative_secondary_monitor_coordinates() {
        let geometry = fit_popup_to_work_area(
            PixelRect { left: -600, top: 1_040, right: -280, bottom: 1_080 },
            PixelRect { left: -1_920, top: 1_040, right: 0, bottom: 1_080 },
            PixelRect { left: -1_920, top: 0, right: 0, bottom: 1_040 },
            880,
            590,
            8,
            8,
        )
        .unwrap();

        assert_eq!(geometry, PopupGeometry { x: -888, y: 442, width: 880, height: 590 });
    }

    #[test]
    fn popup_keeps_dpi_scaled_size_when_typical_work_area_has_room() {
        let cases = [(1_920, 1_040, 880, 590, 8), (2_560, 1_392, 1_320, 885, 12), (3_840, 2_080, 1_760, 1_180, 16)];

        for (work_width, work_height, desired_width, desired_height, margin) in cases {
            let geometry = fit_popup_to_work_area(
                PixelRect {
                    left: work_width / 2 - 160,
                    top: work_height,
                    right: work_width / 2 + 160,
                    bottom: work_height + 40,
                },
                PixelRect { left: 0, top: work_height, right: work_width, bottom: work_height + 40 },
                PixelRect { left: 0, top: 0, right: work_width, bottom: work_height },
                desired_width,
                desired_height,
                margin,
                margin,
            )
            .unwrap();

            assert_eq!((geometry.width, geometry.height), (desired_width, desired_height));
        }
    }

    #[test]
    fn oversized_popup_is_scaled_down_proportionally_and_stays_in_small_work_area() {
        let work = PixelRect { left: 0, top: 0, right: 800, bottom: 500 };
        let geometry = fit_popup_to_work_area(
            PixelRect { left: 240, top: 500, right: 560, bottom: 540 },
            PixelRect { left: 0, top: 500, right: 800, bottom: 540 },
            work,
            1_760,
            1_180,
            16,
            16,
        )
        .unwrap();

        assert_eq!((geometry.width, geometry.height), (698, 468));
        assert!(geometry.width <= 1_760 && geometry.height <= 1_180);
        assert!(geometry.x >= work.left + 16 && geometry.y >= work.top + 16);
        assert!(geometry.x + geometry.width <= work.right - 16);
        assert!(geometry.y + geometry.height <= work.bottom - 16);
        // 像素取整允许至多一个像素误差，但宽高比仍来自同一个缩放因子。
        assert!((geometry.width * 1_180 - geometry.height * 1_760).abs() <= 1_760);
    }
}

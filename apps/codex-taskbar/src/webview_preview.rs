//! WebView2/WebGL 渲染链路的独立验收窗口。
//!
//! 它刻意不接管生产任务栏 HWND：先让已确认的 HTML 原稿在真正的 WebView2
//! Chromium 渲染器中运行，验证 WebGL2、Unicode 文件路径和消息循环没有问题，
//! 再以相同控制器替换 Explorer child window 的 Direct2D 表面。

use std::sync::mpsc;

use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::*,
};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
            GetMessageW, MSG, PostQuitMessage, RegisterClassExW, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY,
            WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
        },
    },
    core::{Error, HSTRING, Result, w},
};

const CLASS_NAME: windows::core::PCWSTR = w!("CodexTaskbarWebViewPreview");

/// 打开实际 WebView2 渲染器中的流体任务栏参考稿。
///
/// 不读取任何账户、SQLite 或配置内容；该入口只验证前端资产与运行时能力。
pub fn run() -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // SAFETY: 此函数拥有本线程完整生命周期，且只初始化一次 STA 公寓。
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }.map_err(|error| stage_error("初始化 COM", error))?;
    let window = create_window().map_err(|error| stage_error("创建预览窗口", error))?;
    // SAFETY: 顶级预览窗口已成功创建；显示不会改变其他应用状态。
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    let environment = create_environment().map_err(|error| stage_error("创建 WebView2 环境", error))?;
    let controller =
        create_controller(&environment, window).map_err(|error| stage_error("创建 WebView2 控制器", error))?;
    let webview = unsafe { controller.CoreWebView2() }.map_err(|error| stage_error("取得 WebView2 页面", error))?;

    // 本地设计稿不需要开发者工具、浏览器右键菜单或缩放入口；生产宿主会沿用此
    // 最小权限集合，并额外添加导航白名单和页面消息校验。
    unsafe {
        let settings = webview.Settings().map_err(|error| stage_error("读取 WebView2 设置", error))?;
        settings
            .SetAreDefaultContextMenusEnabled(false)
            .map_err(|error| stage_error("关闭 WebView2 右键菜单", error))?;
        settings.SetAreDevToolsEnabled(false).map_err(|error| stage_error("关闭 WebView2 开发者工具", error))?;
        settings.SetIsZoomControlEnabled(false).map_err(|error| stage_error("关闭 WebView2 缩放", error))?;
    }
    let mut bounds = RECT::default();
    unsafe {
        GetClientRect(window, &mut bounds).map_err(|error| stage_error("读取预览窗口尺寸", error))?;
        controller.SetBounds(bounds).map_err(|error| stage_error("设置 WebView2 尺寸", error))?;
        controller.SetIsVisible(true).map_err(|error| stage_error("显示 WebView2", error))?;
    }

    let document = HSTRING::from(embedded_prototype_document());
    unsafe { webview.NavigateToString(&document) }.map_err(|error| stage_error("加载流体视觉稿", error))?;
    tracing::info!(event = "webview2_preview_started", "WebView2 流体视觉预览已启动");

    let mut message = MSG::default();
    // SAFETY: 当前线程拥有窗口消息队列；WebView2 的回调也需要这个 STA 循环。
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // controller/webview 在退出循环后在同一 STA 线程析构。
    Ok(())
}

fn stage_error(stage: &str, error: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(format!("{stage}失败：{error}")))
}

fn create_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE(module.0),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // 重复运行时类可能已注册；CreateWindowExW 仍可安全使用已有类。
    let _ = unsafe { RegisterClassExW(&class) };
    unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            w!("Codex Taskbar · WebView2 流体视觉验收"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1120,
            700,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }
}

fn create_environment() -> std::result::Result<ICoreWebView2Environment, webview2_com::Error> {
    let (sender, receiver) = mpsc::sync_channel(1);
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(|handler| unsafe {
            CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |result, environment| {
            result?;
            let _ = sender.send(environment.ok_or_else(|| Error::from(E_POINTER)));
            Ok(())
        }),
    )?;
    Ok(receiver.recv().map_err(|_| webview2_com::Error::WindowsError(Error::from(E_POINTER)))??)
}

fn create_controller(
    environment: &ICoreWebView2Environment,
    parent: HWND,
) -> std::result::Result<ICoreWebView2Controller, webview2_com::Error> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let environment = environment.clone();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            environment.CreateCoreWebView2Controller(parent, &handler).map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |result, controller| {
            result?;
            let _ = sender.send(controller.ok_or_else(|| Error::from(E_POINTER)));
            Ok(())
        }),
    )?;
    Ok(receiver.recv().map_err(|_| webview2_com::Error::WindowsError(Error::from(E_POINTER)))??)
}

fn embedded_prototype_document() -> String {
    const PAGE: &str = include_str!("../../../prototypes/fluid-front-reference.html");
    const CONTRACT: &str = include_str!("../../../prototypes/taskbar-visual-contract.js");
    // `NavigateToString` 没有文件基址，外部 script 不会被加载。把已经确定的视觉
    // 契约内联后，预览与未来打包资源都能使用同一份 HTML，而不是复制一套样式。
    PAGE.replacen("<script src=\"taskbar-visual-contract.js\"></script>", &format!("<script>{CONTRACT}</script>"), 1)
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_DESTROY {
        // SAFETY: WM_DESTROY 表示本线程唯一预览顶级窗口正销毁，退出其消息循环。
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

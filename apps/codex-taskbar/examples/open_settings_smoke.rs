//! 仅供本地 UI 验收：向已经运行的任务栏宿主发送“设置…”菜单命令。

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::{
                EnumChildWindows, EnumWindows, GWL_EXSTYLE, GetClassNameW, GetDesktopWindow, GetWindowLongW,
                SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowLongW, SetWindowPos,
                WM_COMMAND, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
            },
        },
        core::BOOL,
    };

    const MENU_SHOW_DETAILS: usize = 1_001;
    const MENU_OPEN_SETTINGS: usize = 1_002;
    const MENU_EXIT: usize = 1_007;
    static FOUND: AtomicIsize = AtomicIsize::new(0);
    static SETTINGS_FOUND: AtomicIsize = AtomicIsize::new(0);
    static DETAILS_FOUND: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn find_host(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        let mut class_name = [0_u16; 128];
        let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if length > 0 && String::from_utf16_lossy(&class_name[..length as usize]) == "CodexTaskbarNativeHost" {
            FOUND.store(hwnd.0 as isize, Ordering::Release);
            return BOOL(0);
        }
        BOOL(1)
    }

    unsafe extern "system" fn find_settings(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        let mut class_name = [0_u16; 128];
        let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if length > 0 && String::from_utf16_lossy(&class_name[..length as usize]) == "CodexTaskbarSettingsWebView" {
            SETTINGS_FOUND.store(hwnd.0 as isize, Ordering::Release);
            return BOOL(0);
        }
        BOOL(1)
    }

    unsafe extern "system" fn find_details(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        let mut class_name = [0_u16; 128];
        let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if length > 0 && String::from_utf16_lossy(&class_name[..length as usize]) == "CodexTaskbarDetailsCard" {
            DETAILS_FOUND.store(hwnd.0 as isize, Ordering::Release);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mode = std::env::args().nth(1).unwrap_or_else(|| "open".to_owned());
    if mode == "expose-details" {
        unsafe {
            let _ = EnumWindows(Some(find_details), LPARAM(0));
        }
        let raw = DETAILS_FOUND.load(Ordering::Acquire);
        if raw == 0 {
            return Err(windows::core::Error::new(windows::core::HRESULT(0x8007_0490_u32 as i32), "未找到详情卡"));
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        let current = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        unsafe {
            SetWindowLongW(hwnd, GWL_EXSTYLE, ((current & !WS_EX_TOOLWINDOW.0) | WS_EX_APPWINDOW.0) as i32);
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
        return Ok(());
    }
    if mode == "expose" || mode == "restore" {
        unsafe {
            let _ = EnumWindows(Some(find_settings), LPARAM(0));
        }
        let raw = SETTINGS_FOUND.load(Ordering::Acquire);
        if raw == 0 {
            return Err(windows::core::Error::new(windows::core::HRESULT(0x8007_0490_u32 as i32), "未找到设置窗口"));
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        let current = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        let next = if mode == "expose" {
            (current & !WS_EX_TOOLWINDOW.0) | WS_EX_APPWINDOW.0
        } else {
            (current & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0
        };
        unsafe {
            SetWindowLongW(hwnd, GWL_EXSTYLE, next as i32);
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
        return Ok(());
    }

    unsafe {
        let _ = EnumChildWindows(Some(GetDesktopWindow()), Some(find_host), LPARAM(0));
    }
    let raw = FOUND.load(Ordering::Acquire);
    if raw == 0 {
        return Err(windows::core::Error::new(windows::core::HRESULT(0x8007_0490_u32 as i32), "未找到任务栏宿主"));
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    unsafe {
        let command = match mode.as_str() {
            "details" => MENU_SHOW_DETAILS,
            "exit" => MENU_EXIT,
            _ => MENU_OPEN_SETTINGS,
        };
        let _ = SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(command)), Some(LPARAM(0)));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}

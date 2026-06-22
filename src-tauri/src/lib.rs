use chrono::{Datelike, Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Position, Size, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, Window,
};

const SETTINGS_TITLE: &str = "Codex Quota Glance 设置";
const UPDATE_TITLE: &str = "Codex Quota Glance 更新";
const GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/akitten-cn/codex-quota-glance/releases/latest";
const QUOTA_UNITS_PER_CNY: f64 = 500_000.0;
const INITIAL_SYNC_START: i64 = 1_780_243_200;
const SYNC_OVERLAP_SECONDS: i64 = 300;
const CAPSULE_WINDOW_PAD: u32 = 24;
const RESERVED_TOAST_SPACE: u32 = 56;
const DETAIL_GAP: i32 = 10;
const SCREEN_MARGIN: i32 = 8;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateWindowPayload {
    auto_download: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalApiRequest {
    url: String,
    method: Option<String>,
    headers: Option<HashMap<String, String>>,
    body: Option<String>,
}

#[tauri::command]
async fn local_api_request(request: LocalApiRequest) -> Result<Value, String> {
    let method = request
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let path = local_api_path(&request.url)?;
    if path == "/newapi-proxy" {
        return local_api_newapi_proxy(&request, &method).await;
    }
    let _headers = request.headers.as_ref();
    let body = match (method.as_str(), path.as_str()) {
        ("GET", "/local-api/health") => json!({
            "ok": true,
            "backend": "tauri-local-api"
        }),
        ("GET", "/local-api/update/latest") => local_api_update_latest().await,
        ("GET", "/local-api/codex/status") => local_api_codex_status(),
        ("GET", "/local-api/codex/token/latest") => get_latest_codex_token_usage(),
        ("GET", "/local-api/codex/token/summary") => get_codex_token_summary(),
        ("GET", "/local-api/newapi/logs/summary") => local_api_newapi_summary(),
        ("POST", "/local-api/newapi/logs/sync") => {
            sync_newapi_logs(request.body.as_deref().unwrap_or("{}")).await
        }
        ("POST", "/local-api/newapi/diagnose") => {
            diagnose_newapi_user_self(request.body.as_deref().unwrap_or("{}")).await
        }
        _ => {
            return Ok(json!({
                "status": 404,
                "ok": false,
                "headers": {
                    "content-type": "application/json"
                },
                "body": {
                    "ok": false,
                    "message": format!("Tauri local API 尚未实现：{} {}", method, path)
                }
            }));
        }
    };

    Ok(json!({
        "status": 200,
        "ok": true,
        "headers": {
            "content-type": "application/json"
        },
        "body": body
    }))
}

#[tauri::command]
fn desktop_drag_start(window: Window, _point: Value) -> Result<(), String> {
    window.start_dragging().map_err(|error| error.to_string())
}

#[tauri::command]
fn desktop_drag_move(_point: Value) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_drag_end() -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_hit_test_regions(window: Window, payload: Value) -> Result<(), String> {
    let interactive = payload
        .get("interactive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    window
        .set_ignore_cursor_events(!interactive)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn desktop_toast_open(_open: bool) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_layout_update(app: AppHandle, window: Window, layout: Value) -> Result<(), String> {
    let capsule = layout.get("capsule").unwrap_or(&Value::Null);
    let capsule_width = value_u32(capsule.get("width"), 160).max(1);
    let capsule_height = value_u32(capsule.get("height"), 44).max(1);
    let toast_height = layout
        .get("toast")
        .and_then(|toast| toast.get("height"))
        .map(|value| value_u32(Some(value), 0))
        .unwrap_or(0);
    let width = capsule_width + CAPSULE_WINDOW_PAD;
    let height = capsule_height + RESERVED_TOAST_SPACE.max(toast_height) + CAPSULE_WINDOW_PAD;
    window
        .set_size(Size::Physical(PhysicalSize { width, height }))
        .map_err(|error| error.to_string())?;
    emit_window_layout(&app, "bottom")?;
    let _ = position_detail_window(&app);
    Ok(())
}

#[tauri::command]
fn desktop_detail_layout_update(
    app: AppHandle,
    window: Window,
    layout: Value,
) -> Result<(), String> {
    let width = value_u32(layout.get("width"), 520).max(320);
    let height = value_u32(layout.get("height"), 180).max(120);
    window
        .set_size(Size::Physical(PhysicalSize { width, height }))
        .map_err(|error| error.to_string())?;
    let _ = position_detail_window(&app);
    Ok(())
}

#[tauri::command]
fn desktop_saved_position(window: Window, position: Value) -> Result<(), String> {
    let x = value_i32(position.get("x"), 0);
    let y = value_i32(position.get("y"), 0);
    window
        .set_position(Position::Physical(PhysicalPosition { x, y }))
        .map_err(|error| error.to_string())
}

fn position_detail_window(app: &AppHandle) -> Result<(), String> {
    let Some(capsule) = app.get_webview_window("capsule") else {
        return Ok(());
    };
    let Some(detail) = app.get_webview_window("detail") else {
        return Ok(());
    };
    let capsule_position = capsule
        .outer_position()
        .map_err(|error| error.to_string())?;
    let capsule_size = capsule.outer_size().map_err(|error| error.to_string())?;
    let detail_size = detail.outer_size().map_err(|error| error.to_string())?;
    let monitor = capsule
        .current_monitor()
        .map_err(|error| error.to_string())?;
    let (screen_x, screen_y, screen_width, screen_height) = monitor
        .map(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            (
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
            )
        })
        .unwrap_or((0, 0, 1920, 1080));
    let detail_width = detail_size.width as i32;
    let detail_height = detail_size.height as i32;
    let capsule_width = capsule_size.width as i32;
    let capsule_height = capsule_size.height as i32;
    let below_space = screen_y + screen_height - (capsule_position.y + capsule_height);
    let placement = if below_space >= detail_height + DETAIL_GAP + SCREEN_MARGIN {
        "bottom"
    } else {
        "top"
    };
    let x = clamp_i32(
        capsule_position.x + (capsule_width - detail_width) / 2,
        screen_x + SCREEN_MARGIN,
        screen_x + screen_width - detail_width - SCREEN_MARGIN,
    );
    let y = if placement == "bottom" {
        capsule_position.y + capsule_height + DETAIL_GAP
    } else {
        capsule_position.y - detail_height - DETAIL_GAP
    };
    let y = clamp_i32(
        y,
        screen_y + SCREEN_MARGIN,
        screen_y + screen_height - detail_height - SCREEN_MARGIN,
    );
    detail
        .set_position(Position::Physical(PhysicalPosition { x, y }))
        .map_err(|error| error.to_string())?;
    emit_window_layout(app, placement)?;
    Ok(())
}

#[tauri::command]
fn desktop_detail_open(app: AppHandle, open: bool) -> Result<(), String> {
    let detail = ensure_detail_window(&app)?;
    if open {
        detail.show().map_err(|error| error.to_string())?;
        detail.set_focus().map_err(|error| error.to_string())?;
    } else {
        detail.hide().map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn desktop_update_ready(window: Window) -> Result<(), String> {
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())
}

#[tauri::command]
fn desktop_update_dismiss(app: AppHandle) -> Result<(), String> {
    emit_all(&app, "desktop-update-dismissed", json!(null))?;
    if let Some(update) = app.get_webview_window("update") {
        update.close().map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn desktop_update_open_release(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://github.com/akitten-cn/codex-quota-glance/releases") {
        return Err("不受信任的更新地址".to_string());
    }
    tauri_plugin_opener::open_url(url, None::<String>).map_err(|error| error.to_string())?;
    app.emit("desktop-update-release-opened", json!(null))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn desktop_update_open_window(
    app: AppHandle,
    payload: Option<UpdateWindowPayload>,
) -> Result<(), String> {
    let auto_download = payload
        .and_then(|value| value.auto_download)
        .unwrap_or(false);
    let update = ensure_update_window(&app, auto_download)?;
    update.show().map_err(|error| error.to_string())?;
    update.set_focus().map_err(|error| error.to_string())?;
    if auto_download {
        update
            .emit("desktop-update-auto-download", json!(null))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn desktop_update_download(app: AppHandle, asset: Value) -> Result<(), String> {
    let (name, url, expected_size) = trusted_update_asset(&asset)?;
    let target_dir = std::env::temp_dir()
        .join("CodexQuotaGlance")
        .join("updates");
    fs::create_dir_all(&target_dir).map_err(|error| error.to_string())?;
    let target = target_dir.join(safe_file_name(&name));

    emit_all(
        &app,
        "desktop-update-download-progress",
        json!({
            "status": "downloading",
            "message": "正在下载 Tauri 安装包...",
            "received": 0,
            "total": expected_size,
            "percent": 0
        }),
    )?;

    let response = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "CodexQuotaGlance/0.1")
        .send()
        .await
        .map_err(|error| format!("下载安装包失败：{}", error))?;
    if !response.status().is_success() {
        return Err(format!(
            "下载安装包失败：HTTP {}",
            response.status().as_u16()
        ));
    }
    let total = response.content_length().or(expected_size).unwrap_or(0);
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取安装包失败：{}", error))?;
    fs::write(&target, &bytes).map_err(|error| format!("保存安装包失败：{}", error))?;
    let received = bytes.len() as u64;

    emit_all(
        &app,
        "desktop-update-download-progress",
        json!({
            "status": "launching",
            "message": "下载完成，正在关闭当前程序并启动安装程序...",
            "received": received,
            "total": if total > 0 { total } else { received },
            "percent": 100
        }),
    )?;

    std::process::Command::new(&target)
        .spawn()
        .map_err(|error| format!("启动安装程序失败：{}", error))?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn desktop_open_settings(app: AppHandle) -> Result<(), String> {
    let settings = ensure_settings_window(&app)?;
    settings.show().map_err(|error| error.to_string())?;
    settings.set_focus().map_err(|error| error.to_string())
}

fn build_tray_menu(app: &AppHandle) -> tauri::Result<()> {
    let show_hide = MenuItem::with_id(app, "toggle_capsule", "显示/隐藏胶囊", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show_hide, &settings, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Codex Quota Glance")
        .menu(&menu)
        .show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn toggle_capsule_window(app: &AppHandle) -> Result<(), String> {
    if let Some(capsule) = app.get_webview_window("capsule") {
        let visible = capsule.is_visible().map_err(|error| error.to_string())?;
        if visible {
            capsule.hide().map_err(|error| error.to_string())?;
        } else {
            capsule.show().map_err(|error| error.to_string())?;
            capsule.set_focus().map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    Err("未找到胶囊窗口".to_string())
}

fn ensure_detail_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("detail") {
        return Ok(window);
    }
    WebviewWindowBuilder::new(
        app,
        "detail",
        WebviewUrl::App("index.html?view=detail".into()),
    )
    .title("Codex Quota Glance")
    .inner_size(520.0, 180.0)
    .min_inner_size(520.0, 120.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .build()
    .map_err(|error| error.to_string())
}

fn ensure_settings_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("settings") {
        return Ok(window);
    }
    WebviewWindowBuilder::new(
        app,
        "settings",
        WebviewUrl::App("index.html?view=settings".into()),
    )
    .title(SETTINGS_TITLE)
    .inner_size(820.0, 620.0)
    .min_inner_size(560.0, 420.0)
    .resizable(true)
    .visible(false)
    .build()
    .map_err(|error| error.to_string())
}

fn ensure_update_window(app: &AppHandle, auto_download: bool) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("update") {
        return Ok(window);
    }
    let url = if auto_download {
        "index.html?view=update&download=1"
    } else {
        "index.html?view=update"
    };
    WebviewWindowBuilder::new(app, "update", WebviewUrl::App(url.into()))
        .title(UPDATE_TITLE)
        .inner_size(460.0, 320.0)
        .min_inner_size(420.0, 280.0)
        .resizable(false)
        .visible(false)
        .build()
        .map_err(|error| error.to_string())
}

fn emit_all(app: &AppHandle, event: &str, payload: Value) -> Result<(), String> {
    for window in app.webview_windows().values() {
        window
            .emit(event, payload.clone())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn emit_window_layout(app: &AppHandle, placement: &str) -> Result<(), String> {
    let layout = json!({
        "placement": if placement == "top" { "top" } else { "bottom" },
        "offsetX": 0,
        "offsetY": 0,
        "detailOffset": 0,
        "popoverShiftX": 0,
        "ready": true
    });
    emit_all(
        app,
        "desktop-popover-placement",
        json!(layout["placement"].clone()),
    )?;
    emit_all(app, "desktop-window-layout", layout)
}

fn trusted_update_asset(asset: &Value) -> Result<(String, String, Option<u64>), String> {
    let name = asset
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = asset
        .get("url")
        .or_else(|| asset.get("browser_download_url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let lower_name = name.to_ascii_lowercase();
    if !lower_name.ends_with(".exe")
        || !lower_name.contains("win")
        || lower_name.contains("portable")
        || !url.starts_with("https://github.com/akitten-cn/codex-quota-glance/releases/download/")
    {
        return Err("没有找到可信的 Windows 安装包".to_string());
    }
    Ok((name, url, asset.get("size").and_then(Value::as_u64)))
}

fn safe_file_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    if sanitized.trim().is_empty() {
        "CodexQuotaGlance-update.exe".to_string()
    } else {
        sanitized
    }
}

fn local_api_path(url: &str) -> Result<String, String> {
    if url.starts_with("/local-api/") || url == "/newapi-proxy" {
        return Ok(url.split('?').next().unwrap_or(url).to_string());
    }
    if let Some(index) = url.find("/local-api/") {
        return Ok(url[index..]
            .split('?')
            .next()
            .unwrap_or(&url[index..])
            .to_string());
    }
    Err(format!("不支持的本地 API 地址：{}", url))
}

async fn local_api_update_latest() -> Value {
    match reqwest::Client::new()
        .get(GITHUB_LATEST_RELEASE_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "CodexQuotaGlance/0.1")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(release) => json!({
                "ok": true,
                "tag_name": release.get("tag_name").and_then(Value::as_str),
                "html_url": release.get("html_url").and_then(Value::as_str),
                "assets": release
                    .get("assets")
                    .and_then(Value::as_array)
                    .map(|assets| {
                        assets
                            .iter()
                            .map(|asset| {
                                json!({
                                    "name": asset.get("name").and_then(Value::as_str),
                                    "browser_download_url": asset.get("browser_download_url").and_then(Value::as_str),
                                    "size": asset.get("size").and_then(Value::as_u64)
                                })
                            })
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default()
            }),
            Err(error) => json!({
                "ok": false,
                "message": format!("GitHub Releases 解析失败：{}", error)
            }),
        },
        Ok(response) => json!({
            "ok": false,
            "message": format!("GitHub Releases 检查失败：HTTP {}", response.status().as_u16())
        }),
        Err(error) => json!({
            "ok": false,
            "message": format!("GitHub Releases 检查失败：{}", error)
        }),
    }
}

async fn local_api_newapi_proxy(request: &LocalApiRequest, method: &str) -> Result<Value, String> {
    let target = header_value(request.headers.as_ref(), "x-newapi-target");
    if target.trim().is_empty() {
        return Ok(local_api_text_response(400, "Missing X-NewAPI-Target"));
    }
    let target_url =
        reqwest::Url::parse(&target).map_err(|_| "Unsupported target URL".to_string())?;
    if !matches!(target_url.scheme(), "http" | "https") {
        return Ok(local_api_text_response(400, "Unsupported target protocol"));
    }
    let method = method
        .parse::<reqwest::Method>()
        .unwrap_or(reqwest::Method::GET);
    let mut builder = reqwest::Client::new()
        .request(method.clone(), target_url)
        .header("User-Agent", "CodexQuotaGlance/0.1");

    for (name, value) in newapi_proxy_headers(request.headers.as_ref()) {
        builder = builder.header(name, value);
    }
    if method != reqwest::Method::GET {
        if let Some(body) = request.body.as_deref() {
            builder = builder.body(body.to_string());
        }
    }

    let response = builder
        .send()
        .await
        .map_err(|error| format!("New API 代理请求失败：{}", error))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json; charset=utf-8")
        .to_string();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取 New API 响应失败：{}", error))?;
    let body = if content_type.to_ascii_lowercase().contains("json") {
        serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text))
    } else {
        Value::String(text)
    };

    Ok(json!({
        "status": status,
        "ok": status < 400,
        "headers": {
            "content-type": content_type,
            "cache-control": "no-store"
        },
        "body": body
    }))
}

async fn diagnose_newapi_user_self(body: &str) -> Value {
    let payload = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let base_url = trim_trailing_slash(string_field(&payload, "baseUrl"));
    let access_token = string_field(&payload, "accessToken");
    let api_user = string_field(&payload, "newApiUser");
    if base_url.is_empty() || access_token.is_empty() {
        return json!({
            "ok": false,
            "message": "baseUrl and accessToken are required"
        });
    }
    let url = match reqwest::Url::parse(&format!("{}/api/user/self", base_url)) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "ok": false,
                "message": format!("New API 地址无效：{}", error)
            });
        }
    };
    let raw_request = [
        "GET /api/user/self HTTP/1.1".to_string(),
        format!("Host: {}", url.host_str().unwrap_or_default()),
        format!(
            "Authorization: Bearer {}",
            mask_secret(&clean_bearer_token(&access_token))
        ),
        format!("New-Api-User: {}", api_user),
        "User-Agent: Apifox/1.0.0 (https://apifox.com)".to_string(),
        "Accept: */*".to_string(),
        "Connection: keep-alive".to_string(),
    ]
    .join("\r\n");

    let mut http_status = 0_u16;
    let mut parsed_body = Value::Null;
    let message: String;
    let mut builder = reqwest::Client::new().get(url.as_str());
    for (name, value) in newapi_auth_headers(&access_token, &api_user, true) {
        builder = builder.header(name, value);
    }
    match builder.send().await {
        Ok(response) => {
            http_status = response.status().as_u16();
            match response.text().await {
                Ok(text) => {
                    message = text.chars().take(200).collect();
                    parsed_body = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
                }
                Err(error) => {
                    message = error.to_string();
                }
            }
        }
        Err(error) => {
            message = error.to_string();
        }
    }
    let data_keys = parsed_body
        .get("data")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .keys()
                .take(40)
                .map(|key| Value::String(key.to_string()))
                .collect::<Vec<Value>>()
        })
        .unwrap_or_default();

    json!({
        "ok": true,
        "request": {
            "url": url.to_string(),
            "sentHeaders": masked_newapi_headers(&access_token, &api_user, true),
            "rawHttpRequest": raw_request,
            "diagnostics": newapi_header_diagnostics(&access_token)
        },
        "response": {
            "httpStatus": http_status,
            "success": parsed_body.get("success").cloned().unwrap_or(Value::Null),
            "message": parsed_body
                .get("message")
                .and_then(Value::as_str)
                .map(|value| value.to_string())
                .unwrap_or(message),
            "dataKeys": data_keys
        }
    })
}

async fn sync_newapi_logs(body: &str) -> Value {
    match sync_newapi_logs_inner(body).await {
        Ok(value) => value,
        Err(error) => json!({
            "ok": false,
            "message": error,
            "summary": local_api_newapi_summary()
        }),
    }
}

async fn sync_newapi_logs_inner(body: &str) -> Result<Value, String> {
    let payload = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let base_url = trim_trailing_slash(string_field(&payload, "baseUrl"));
    let access_token = string_field(&payload, "accessToken");
    let api_key = string_field(&payload, "apiKey");
    let api_user = string_field(&payload, "newApiUser");
    let token_name = string_field(&payload, "tokenName");
    let sync_secret = if !access_token.trim().is_empty() {
        access_token.trim().to_string()
    } else {
        api_key.trim().to_string()
    };
    if base_url.is_empty() || sync_secret.is_empty() {
        return Err("baseUrl and apiKey/accessToken are required".to_string());
    }

    let database_path = newapi_database_path();
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut connection = Connection::open(&database_path).map_err(|error| error.to_string())?;
    init_newapi_database(&connection)?;
    let sync_key = make_newapi_sync_key(&base_url, &api_user, &sync_secret);
    let latest = get_newapi_latest_created_at(&connection, &sync_key)?.or(query_single_i64(
        &connection,
        "select max(created_at) from newapi_logs",
    )?);
    let start = latest
        .map(|value| (value - SYNC_OVERLAP_SECONDS).max(0))
        .unwrap_or(INITIAL_SYNC_START);
    let end = number_or_option(payload.get("endTimestamp"))
        .map(|value| value.trunc() as i64)
        .unwrap_or_else(|| unix_now_seconds() as i64);
    let page_size = number_or_option(payload.get("pageSize"))
        .map(|value| value.trunc() as i64)
        .unwrap_or(100)
        .clamp(1, 1000);
    let log_window_cap = number_or_option(payload.get("logWindowCap"))
        .map(|value| value.trunc() as usize)
        .unwrap_or(1000);

    let mut mode = if latest.is_some() {
        "incremental"
    } else {
        "initial"
    }
    .to_string();
    let mut fetched = 0_usize;
    let mut page = 1_i64;
    let mut page_limit_reached = false;
    let mut inserted_rows = Vec::new();

    if !access_token.trim().is_empty() {
        loop {
            let data = fetch_newapi_self_log_page(
                &base_url,
                &access_token,
                &api_user,
                &token_name,
                page,
                page_size,
                start,
                end,
            )
            .await?;
            let items = normalize_newapi_log_items(&data);
            if items.is_empty() {
                break;
            }
            fetched += items.len();
            inserted_rows.extend(insert_newapi_logs(&mut connection, &items)?);
            if items.len() < page_size as usize {
                break;
            }
            page += 1;
            if page > 500 {
                page_limit_reached = true;
                break;
            }
        }
    } else {
        mode = "fallback-token".to_string();
        let data = fetch_newapi_token_log_page(&base_url, &api_key, &api_user).await?;
        let items = normalize_newapi_log_items(&data);
        fetched = items.len();
        inserted_rows = insert_newapi_logs(&mut connection, &items)?;
    }

    let newest = query_single_i64(&connection, "select max(created_at) from newapi_logs")?;
    save_newapi_sync_state(
        &connection,
        &sync_key,
        &base_url,
        &api_user,
        &sync_secret,
        newest,
    )?;
    let capped = fetched >= log_window_cap || page_limit_reached;
    let backfill_warning = if capped {
        Some("some log windows reached the platform cap; logs may be truncated")
    } else {
        None
    };

    Ok(json!({
        "ok": true,
        "mode": mode,
        "startTimestamp": start,
        "endTimestamp": end,
        "pages": page,
        "fetched": fetched,
        "inserted": inserted_rows.len(),
        "capped": capped,
        "backfillWarning": backfill_warning,
        "insertedUsage": summarize_newapi_log_rows(&inserted_rows),
        "summary": local_api_newapi_summary()
    }))
}

async fn fetch_newapi_self_log_page(
    base_url: &str,
    access_token: &str,
    api_user: &str,
    token_name: &str,
    page: i64,
    page_size: i64,
    start: i64,
    end: i64,
) -> Result<Value, String> {
    let url = build_newapi_self_log_url(base_url, token_name, page, page_size, start, end)?;
    fetch_newapi_json(&url, access_token, api_user, "log sync failed").await
}

async fn fetch_newapi_token_log_page(
    base_url: &str,
    api_key: &str,
    api_user: &str,
) -> Result<Value, String> {
    let mut url = reqwest::Url::parse(&format!("{}/api/log/token", trim_trailing_slash(base_url)))
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("key", api_key);
    fetch_newapi_json(url.as_str(), api_key, api_user, "token log sync failed").await
}

fn build_newapi_self_log_url(
    base_url: &str,
    token_name: &str,
    page: i64,
    page_size: i64,
    start: i64,
    end: i64,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{}/api/log/self", trim_trailing_slash(base_url)))
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut()
        .append_pair("p", &page.to_string())
        .append_pair("page_size", &page_size.to_string())
        .append_pair("type", "0")
        .append_pair("token_name", token_name)
        .append_pair("model_name", "")
        .append_pair("start_timestamp", &start.to_string())
        .append_pair("end_timestamp", &end.to_string())
        .append_pair("group", "")
        .append_pair("request_id", "");
    Ok(url.to_string())
}

async fn fetch_newapi_json(
    url: &str,
    token: &str,
    api_user: &str,
    label: &str,
) -> Result<Value, String> {
    let mut builder = reqwest::Client::new().get(url);
    for (name, value) in newapi_auth_headers(token, api_user, false) {
        builder = builder.header(name, value);
    }
    let response = builder.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|error| error.to_string())?;
    if status.as_u16() == 429 {
        return Err("rate limited".to_string());
    }
    if status.as_u16() >= 400 {
        return Err(format!(
            "{}: HTTP {} {}",
            label,
            status.as_u16(),
            text.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str::<Value>(&text).map_err(|_| format!("{}: invalid JSON", label))
}

#[derive(Clone)]
struct NewApiLogRow {
    unique_id: String,
    provider_log_id: Option<String>,
    request_id: Option<String>,
    created_at: i64,
    token_name: Option<String>,
    model_name: Option<String>,
    group_name: Option<String>,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    raw_used_amount: i64,
    other_json: Option<String>,
}

fn normalize_newapi_log_items(payload: &Value) -> Vec<Value> {
    if let Some(items) = payload.as_array() {
        return items
            .iter()
            .filter(|item| item.is_object())
            .cloned()
            .collect();
    }
    let data = payload.get("data").unwrap_or(payload);
    if let Some(items) = data.as_array() {
        return items
            .iter()
            .filter(|item| item.is_object())
            .cloned()
            .collect();
    }
    for key in ["items", "logs", "data"] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            return items
                .iter()
                .filter(|item| item.is_object())
                .cloned()
                .collect();
        }
    }
    Vec::new()
}

fn normalize_newapi_log_row(item: &Value) -> NewApiLogRow {
    let other = parse_newapi_other(item.get("other"));
    let request_id = optional_string(item.get("request_id"));
    let provider_log_id = optional_string(item.get("id"));
    let created_at = number_or_zero_i64(item.get("created_at"));
    let input_tokens = first_number_i64(item, &["prompt_tokens", "input_tokens"]);
    let output_tokens = first_number_i64(item, &["completion_tokens", "output_tokens"]);
    let cached_input_tokens = number_or_zero_i64(other.get("cache_tokens"))
        .max(number_or_zero_i64(item.get("cached_tokens")))
        .max(number_or_zero_i64(item.get("cached_input_tokens")));
    let total_tokens = number_or_zero_i64(item.get("total_tokens"));
    let total_tokens = if total_tokens > 0 {
        total_tokens
    } else {
        input_tokens + output_tokens
    };
    let raw_used_amount = first_number_i64(item, &["quota", "used_quota"]);
    let unique_id = request_id
        .as_ref()
        .map(|value| format!("req:{}", value))
        .unwrap_or_else(|| {
            format!(
                "id:{}",
                provider_log_id.clone().unwrap_or_else(|| format!(
                    "{}:{}:{}",
                    created_at, input_tokens, output_tokens
                ))
            )
        });

    NewApiLogRow {
        unique_id,
        provider_log_id,
        request_id,
        created_at,
        token_name: optional_string(item.get("token_name")),
        model_name: optional_string(item.get("model_name")),
        group_name: optional_string(item.get("group")),
        input_tokens,
        cached_input_tokens,
        output_tokens,
        total_tokens,
        raw_used_amount,
        other_json: item
            .get("other")
            .and_then(Value::as_str)
            .map(|value| value.to_string())
            .or_else(|| serde_json::to_string(&other).ok()),
    }
}

fn insert_newapi_logs(
    connection: &mut Connection,
    items: &[Value],
) -> Result<Vec<NewApiLogRow>, String> {
    let rows = items
        .iter()
        .map(normalize_newapi_log_row)
        .collect::<Vec<_>>();
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let mut inserted = Vec::new();
    {
        let mut statement = transaction
            .prepare(
                r#"
                insert or ignore into newapi_logs (
                  unique_id, provider_log_id, request_id, created_at, token_name, model_name,
                  group_name, input_tokens, cached_input_tokens, output_tokens, total_tokens,
                  raw_used_amount, other_json
                ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                "#,
            )
            .map_err(|error| error.to_string())?;
        for row in rows {
            let changes = statement
                .execute(params![
                    &row.unique_id,
                    &row.provider_log_id,
                    &row.request_id,
                    row.created_at,
                    &row.token_name,
                    &row.model_name,
                    &row.group_name,
                    row.input_tokens,
                    row.cached_input_tokens,
                    row.output_tokens,
                    row.total_tokens,
                    row.raw_used_amount,
                    &row.other_json
                ])
                .map_err(|error| error.to_string())?;
            if changes > 0 {
                inserted.push(row);
            }
        }
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(inserted)
}

fn summarize_newapi_log_rows(rows: &[NewApiLogRow]) -> Value {
    let request_count = rows.len() as i64;
    let input_tokens: i64 = rows.iter().map(|row| row.input_tokens).sum();
    let cached_input_tokens: i64 = rows.iter().map(|row| row.cached_input_tokens).sum();
    let output_tokens: i64 = rows.iter().map(|row| row.output_tokens).sum();
    let total_tokens: i64 = rows.iter().map(|row| row.total_tokens).sum();
    let raw_used_amount: i64 = rows.iter().map(|row| row.raw_used_amount).sum();
    let latest_created_at = rows.iter().map(|row| row.created_at).max();
    let cache_hit_rate = if input_tokens > 0 {
        Some((cached_input_tokens as f64 / input_tokens as f64) * 100.0)
    } else {
        None
    };
    json!({
        "requestCount": request_count,
        "inputTokens": input_tokens,
        "cachedInputTokens": cached_input_tokens,
        "outputTokens": output_tokens,
        "totalTokens": total_tokens,
        "rawUsedAmount": raw_used_amount,
        "usedAmount": raw_used_amount as f64 / QUOTA_UNITS_PER_CNY,
        "cacheHitRate": cache_hit_rate,
        "latestCreatedAt": latest_created_at
    })
}

fn save_newapi_sync_state(
    connection: &Connection,
    sync_key: &str,
    base_url: &str,
    api_user: &str,
    secret: &str,
    latest: Option<i64>,
) -> Result<(), String> {
    connection
        .execute(
            r#"
            insert into newapi_sync_state (
              sync_key, base_url, api_user, key_fingerprint, latest_created_at, last_synced_at,
              fail_count, blocked_until, backfill_until
            ) values (?1, ?2, ?3, ?4, ?5, ?6, 0, null, null)
            on conflict(sync_key) do update set
              latest_created_at = excluded.latest_created_at,
              last_synced_at = excluded.last_synced_at,
              fail_count = 0,
              blocked_until = null
            "#,
            params![
                sync_key,
                base_url,
                api_user,
                fnv1a_hex(secret),
                latest,
                unix_now_seconds() as i64
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn get_newapi_latest_created_at(
    connection: &Connection,
    sync_key: &str,
) -> Result<Option<i64>, String> {
    connection
        .query_row(
            "select latest_created_at from newapi_sync_state where sync_key = ?1",
            params![sync_key],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(|error| error.to_string())
}

fn make_newapi_sync_key(base_url: &str, api_user: &str, secret: &str) -> String {
    fnv1a_hex(&format!(
        "{}:{}:{}",
        trim_trailing_slash(base_url.to_string()),
        api_user.trim(),
        secret.trim()
    ))
}

fn get_latest_codex_token_usage() -> Value {
    let Some(session_file) = find_latest_codex_session_file() else {
        return json!({
            "ok": true,
            "available": false
        });
    };

    let latest_event = match read_latest_codex_token_event(&session_file) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "ok": false,
                "available": false,
                "message": error
            });
        }
    };

    let Some(event) = latest_event else {
        return json!({
            "ok": true,
            "available": false,
            "sessionFile": session_file.to_string_lossy().to_string()
        });
    };

    codex_token_event_payload(&session_file, &event)
}

fn read_latest_codex_token_event(session_file: &Path) -> Result<Option<Value>, String> {
    let content = fs::read_to_string(session_file).map_err(|error| error.to_string())?;
    let mut latest_event = None;
    for line in content.lines() {
        if !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = event.get("payload").unwrap_or(&Value::Null);
        if string_field(payload, "type") == "token_count" {
            latest_event = Some(event);
        }
    }
    Ok(latest_event)
}

fn get_codex_token_summary() -> Value {
    let account_type = get_codex_account_type();
    let rows = read_codex_token_rows();
    let today = current_utc_date_string();
    let today_rows = rows
        .iter()
        .filter(|row| row.timestamp.starts_with(&today))
        .cloned()
        .collect::<Vec<CodexTokenRow>>();
    let latest_event_at = rows
        .iter()
        .filter_map(|row| {
            parse_iso_timestamp_seconds(&row.timestamp)
                .map(|seconds| (seconds, row.timestamp.clone()))
        })
        .max_by_key(|(seconds, _)| *seconds)
        .map(|(_, timestamp)| timestamp);

    json!({
        "ok": true,
        "accountType": account_type,
        "today": summarize_codex_token_rows(&today_rows),
        "all": summarize_codex_token_rows(&rows),
        "latestEventAt": latest_event_at
    })
}

#[derive(Clone)]
struct CodexTokenRow {
    timestamp: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    reasoning_output_tokens: u64,
}

fn read_codex_token_rows() -> Vec<CodexTokenRow> {
    let mut files = Vec::new();
    collect_jsonl_files(&codex_home_dir().join("sessions"), &mut files);
    let mut rows = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        for line in content.lines() {
            if !line.contains("\"token_count\"") {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(row) = codex_token_row_from_event(&event) else {
                continue;
            };
            rows.push(row);
        }
    }
    rows
}

fn codex_token_row_from_event(event: &Value) -> Option<CodexTokenRow> {
    let payload = event.get("payload")?;
    if string_field(payload, "type") != "token_count" {
        return None;
    }
    let usage = payload
        .get("info")
        .and_then(|info| info.get("last_token_usage"))
        .unwrap_or(&Value::Null);
    Some(CodexTokenRow {
        timestamp: string_field(event, "timestamp"),
        input_tokens: number_or_zero(usage.get("input_tokens")),
        cached_input_tokens: number_or_zero(usage.get("cached_input_tokens")),
        output_tokens: number_or_zero(usage.get("output_tokens")),
        total_tokens: number_or_zero(usage.get("total_tokens")),
        reasoning_output_tokens: number_or_zero(usage.get("reasoning_output_tokens")),
    })
}

fn codex_token_event_payload(session_file: &Path, event: &Value) -> Value {
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let usage = payload
        .get("info")
        .and_then(|info| info.get("last_token_usage"))
        .unwrap_or(&Value::Null);
    let rate_limits = payload.get("rate_limits").unwrap_or(&Value::Null);
    let timestamp = string_field(event, "timestamp");
    let session_file_text = session_file.to_string_lossy().to_string();
    let event_fingerprint = format!(
        "{}:{}:{}:{}",
        session_file_text,
        timestamp,
        serde_json::to_string(usage).unwrap_or_default(),
        serde_json::to_string(rate_limits).unwrap_or_default()
    );

    json!({
        "ok": true,
        "available": true,
        "source": "codex",
        "eventId": fnv1a_hex(&event_fingerprint),
        "timestamp": timestamp,
        "accountType": get_codex_account_type(),
        "sessionFile": session_file_text,
        "usage": {
            "inputTokens": number_or_zero(usage.get("input_tokens")),
            "cachedInputTokens": number_or_zero(usage.get("cached_input_tokens")),
            "outputTokens": number_or_zero(usage.get("output_tokens")),
            "totalTokens": number_or_zero(usage.get("total_tokens")),
            "reasoningOutputTokens": number_or_zero(usage.get("reasoning_output_tokens"))
        },
        "quota": normalize_codex_rate_limits(rate_limits)
    })
}

fn summarize_codex_token_rows(rows: &[CodexTokenRow]) -> Value {
    let request_count = rows.len() as u64;
    let input_tokens: u64 = rows.iter().map(|row| row.input_tokens).sum();
    let cached_input_tokens: u64 = rows.iter().map(|row| row.cached_input_tokens).sum();
    let output_tokens: u64 = rows.iter().map(|row| row.output_tokens).sum();
    let total_tokens: u64 = rows.iter().map(|row| row.total_tokens).sum();
    let latest_log_at = rows
        .iter()
        .filter_map(|row| {
            parse_iso_timestamp_seconds(&row.timestamp)
                .map(|seconds| (seconds, row.timestamp.clone()))
        })
        .max_by_key(|(seconds, _)| *seconds)
        .map(|(_, timestamp)| timestamp);
    let cache_hit_rate = if input_tokens > 0 {
        Some((cached_input_tokens as f64 / input_tokens as f64) * 100.0)
    } else {
        None
    };

    json!({
        "requestCount": request_count,
        "inputTokens": input_tokens,
        "cachedInputTokens": cached_input_tokens,
        "outputTokens": output_tokens,
        "totalTokens": total_tokens,
        "reasoningOutputTokens": rows.iter().map(|row| row.reasoning_output_tokens).sum::<u64>(),
        "rawUsedAmount": 0,
        "usedAmount": 0,
        "cacheHitRate": cache_hit_rate,
        "latestLogAt": latest_log_at
    })
}

fn normalize_codex_rate_limits(rate_limits: &Value) -> Value {
    if !rate_limits.is_object() {
        return json!({});
    }
    json!({
        "window5h": normalize_codex_rate_limit_window(rate_limits.get("primary").unwrap_or(&Value::Null)),
        "weekly": normalize_codex_rate_limit_window(rate_limits.get("secondary").unwrap_or(&Value::Null)),
        "planType": string_or_null(rate_limits.get("plan_type")),
        "rateLimitReachedType": string_or_null(rate_limits.get("rate_limit_reached_type"))
    })
}

fn normalize_codex_rate_limit_window(window: &Value) -> Value {
    if !window.is_object() {
        return json!({});
    }
    let used_percent = number_or_option(window.get("used_percent"));
    let resets_at = number_or_option(window.get("resets_at"));
    let remaining_percent = used_percent.map(|value| (100.0 - value).clamp(0.0, 100.0));
    let reset_in_seconds = resets_at.map(|value| {
        let now = unix_now_seconds() as f64;
        (value - now).max(0.0).trunc() as u64
    });
    json!({
        "usedPercent": used_percent,
        "remainingPercent": remaining_percent,
        "windowMinutes": number_or_option(window.get("window_minutes")),
        "resetAt": resets_at.map(unix_seconds_to_iso),
        "resetInSeconds": reset_in_seconds
    })
}

fn get_codex_account_type() -> &'static str {
    codex_account_type()
}

fn local_api_codex_status() -> Value {
    let codex_home = codex_home_dir();
    let auth_exists = codex_home.join("auth.json").exists();
    let config = codex_home.join("config.toml");
    json!({
        "ok": true,
        "accountType": if auth_exists { "official_login" } else { "api" },
        "quota": {},
        "activity": get_latest_codex_activity(),
        "source": config.to_string_lossy(),
        "updatedAt": chrono_like_now()
    })
}

fn get_latest_codex_activity() -> Value {
    match find_latest_codex_session_file() {
        Some(session_file) => {
            let mut activity = parse_codex_activity(&session_file);
            if let Some(object) = activity.as_object_mut() {
                object.insert(
                    "sessionFile".to_string(),
                    Value::String(session_file.to_string_lossy().to_string()),
                );
            }
            activity
        }
        None => json!({
            "status": "unknown",
            "label": "未读取到 Codex 会话",
            "needsHumanAttention": false,
            "completedTask": false
        }),
    }
}

fn parse_codex_activity(session_file: &Path) -> Value {
    let content = match fs::read_to_string(session_file) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "status": "unknown",
                "label": format!("读取 Codex 会话失败：{}", error),
                "needsHumanAttention": false,
                "completedTask": false
            });
        }
    };
    let tail_start = content.len().saturating_sub(512 * 1024);
    let tail = &content[tail_start..];
    let mut activity: Option<Value> = None;
    for line in tail.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some((status, needs_human_attention, completed_task)) = codex_activity_update(&event)
        else {
            continue;
        };
        activity = Some(json!({
            "status": status,
            "label": codex_activity_label(status, needs_human_attention),
            "timestamp": event.get("timestamp").and_then(Value::as_str),
            "needsHumanAttention": needs_human_attention,
            "completedTask": completed_task
        }));
    }
    activity.unwrap_or_else(|| {
        json!({
            "status": "finished",
            "label": "空闲",
            "needsHumanAttention": false,
            "completedTask": false
        })
    })
}

fn codex_activity_update(event: &Value) -> Option<(&'static str, bool, bool)> {
    let event_type = string_field(event, "type");
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let payload_type = string_field(payload, "type");

    if contains_human_waiting_signal(payload) || contains_human_review_signal(payload) {
        return Some(("waiting_for_user", true, false));
    }
    if contains_auto_review_signal(payload) {
        return Some(("auto_reviewing", false, false));
    }
    if is_tool_start_event(&event_type, &payload_type, payload) {
        return Some(("executing", false, false));
    }
    if event_type == "event_msg" {
        return match payload_type.as_str() {
            "task_started" | "user_message" | "agent_message_delta" | "patch_apply_end" => {
                Some(("thinking", false, false))
            }
            "task_complete" => Some(("finished", false, true)),
            "turn_aborted" | "thread_rolled_back" => Some(("finished", false, false)),
            "patch_apply_begin" => Some(("executing", false, false)),
            "agent_message" if contains_plan_choice_signal(payload) => {
                Some(("waiting_for_user", true, false))
            }
            "agent_message" => Some(("thinking", false, false)),
            "" => None,
            _ => Some(("thinking", false, false)),
        };
    }
    if event_type == "response_item" {
        if contains_plan_choice_signal(payload) {
            return Some(("waiting_for_user", true, false));
        }
        return match payload_type.as_str() {
            "function_call" => {
                if function_call_needs_user(payload) {
                    Some(("waiting_for_user", true, false))
                } else {
                    Some(("executing", false, false))
                }
            }
            "function_call_output" | "custom_tool_call_output" | "reasoning" | "message" => {
                Some(("thinking", false, false))
            }
            "" => None,
            _ => Some(("thinking", false, false)),
        };
    }
    None
}

fn find_latest_codex_session_file() -> Option<PathBuf> {
    let sessions = codex_home_dir().join("sessions");
    let mut files = Vec::new();
    collect_jsonl_files(&sessions, &mut files);
    files.sort_by(|a, b| {
        let b_modified = fs::metadata(b).and_then(|meta| meta.modified()).ok();
        let a_modified = fs::metadata(a).and_then(|meta| meta.modified()).ok();
        b_modified.cmp(&a_modified)
    });
    files.into_iter().next()
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn codex_activity_label(status: &str, needs_human_attention: bool) -> &'static str {
    match status {
        "thinking" => "思考中",
        "executing" | "answering" => "执行中",
        "waiting_for_user" if needs_human_attention => "等待授权",
        "waiting_for_user" => "思考中",
        "auto_reviewing" => "自动审核中",
        "finished" => "空闲",
        _ => "未知",
    }
}

fn function_call_needs_user(payload: &Value) -> bool {
    let name = string_field(payload, "name");
    if name == "request_user_input" || name == "request_plugin_install" {
        return true;
    }
    let arguments = string_field(payload, "arguments").to_ascii_lowercase();
    arguments.contains("require_escalated") || arguments.contains("sandbox_permissions")
}

fn is_tool_start_event(event_type: &str, payload_type: &str, payload: &Value) -> bool {
    if function_call_needs_user(payload) {
        return false;
    }
    [
        "function_call",
        "custom_tool_call",
        "web_search_call",
        "patch_apply_begin",
    ]
    .contains(&event_type)
        || [
            "function_call",
            "custom_tool_call",
            "web_search_call",
            "patch_apply_begin",
        ]
        .contains(&payload_type)
        || ["apply_patch", "exec_command", "write_stdin", "view_image"]
            .contains(&string_field(payload, "name").as_str())
}

fn contains_human_waiting_signal(payload: &Value) -> bool {
    ["type", "name"]
        .iter()
        .map(|key| string_field(payload, key).to_ascii_lowercase())
        .any(|value| {
            value.contains("approval")
                || value.contains("permission")
                || value.contains("request_user_input")
        })
        || function_call_needs_user(payload)
}

fn contains_auto_review_signal(payload: &Value) -> bool {
    structured_string_values(payload).iter().any(|value| {
        value
            .replace('-', "_")
            .to_ascii_lowercase()
            .contains("auto_review")
    })
}

fn contains_human_review_signal(payload: &Value) -> bool {
    if contains_auto_review_signal(payload) {
        return false;
    }
    structured_string_values(payload).iter().any(|value| {
        let text = value.replace('-', "_").to_ascii_lowercase();
        text.contains("review_pending") || text.contains("reviewing") || text.contains("reviewer")
    })
}

fn contains_plan_choice_signal(payload: &Value) -> bool {
    string_values(payload).iter().any(|value| {
        let lower = value.to_ascii_lowercase();
        lower.contains("<proposed_plan>") || value.contains("实施此计划")
    })
}

fn structured_string_values(payload: &Value) -> Vec<String> {
    [
        "type",
        "name",
        "status",
        "reviewer",
        "approval_reviewer",
        "approvals_reviewer",
    ]
    .iter()
    .map(|key| string_field(payload, key))
    .filter(|value| !value.is_empty())
    .collect()
}

fn string_values(value: &Value) -> Vec<String> {
    let mut values = Vec::new();
    collect_string_values(value, &mut values);
    values
}

fn collect_string_values(value: &Value, values: &mut Vec<String>) {
    match value {
        Value::String(text) => values.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_string_values(item, values);
            }
        }
        Value::Object(object) => {
            for item in object.values() {
                collect_string_values(item, values);
            }
        }
        _ => {}
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn header_value(headers: Option<&HashMap<String, String>>, name: &str) -> String {
    headers
        .and_then(|items| {
            items
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_string())
        })
        .unwrap_or_default()
}

fn newapi_proxy_headers(headers: Option<&HashMap<String, String>>) -> Vec<(&'static str, String)> {
    let mut result = Vec::new();
    let authorization = header_value(headers, "authorization");
    if !authorization.trim().is_empty() {
        result.push(("Authorization", authorization));
    }
    let api_user = header_value(headers, "new-api-user");
    if !api_user.trim().is_empty() {
        result.push(("New-Api-User", api_user));
    }
    let accept = header_value(headers, "accept");
    result.push((
        "Accept",
        if accept.trim().is_empty() {
            "application/json".to_string()
        } else {
            accept
        },
    ));
    result
}

fn newapi_auth_headers(
    token: &str,
    api_user: &str,
    apifox_like: bool,
) -> Vec<(&'static str, String)> {
    let mut result = vec![
        (
            "Authorization",
            format!("Bearer {}", clean_bearer_token(token)),
        ),
        ("New-Api-User", api_user.trim().to_string()),
    ];
    if apifox_like {
        result.push((
            "User-Agent",
            "Apifox/1.0.0 (https://apifox.com)".to_string(),
        ));
        result.push(("Accept", "*/*".to_string()));
        result.push(("Connection", "keep-alive".to_string()));
    } else {
        result.push(("Accept", "application/json".to_string()));
    }
    result
        .into_iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .collect()
}

fn masked_newapi_headers(token: &str, api_user: &str, apifox_like: bool) -> Value {
    let mut headers = serde_json::Map::new();
    headers.insert(
        "Authorization".to_string(),
        Value::String(format!(
            "Bearer {}",
            mask_secret(&clean_bearer_token(token))
        )),
    );
    headers.insert(
        "New-Api-User".to_string(),
        Value::String(api_user.trim().to_string()),
    );
    if apifox_like {
        headers.insert(
            "User-Agent".to_string(),
            Value::String("Apifox/1.0.0 (https://apifox.com)".to_string()),
        );
        headers.insert("Accept".to_string(), Value::String("*/*".to_string()));
    }
    Value::Object(headers)
}

fn newapi_header_diagnostics(token: &str) -> Value {
    let clean = clean_bearer_token(token);
    json!({
        "tokenTrimmedLength": clean.len(),
        "tokenHashPrefix": fnv1a_hex(&clean).chars().take(12).collect::<String>(),
        "authorizationPrefix": if clean.is_empty() { "" } else { "Bearer " }
    })
}

fn clean_bearer_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.to_ascii_lowercase().starts_with("bearer ") {
        trimmed
            .char_indices()
            .nth(7)
            .map(|(index, _)| trimmed[index..].trim().to_string())
            .unwrap_or_default()
    } else {
        trimmed.to_string()
    }
}

fn mask_secret(secret: &str) -> String {
    let chars = secret.chars().collect::<Vec<char>>();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let prefix = chars.iter().take(4).collect::<String>();
    let suffix = chars
        .iter()
        .skip(chars.len().saturating_sub(4))
        .collect::<String>();
    format!("{}…{}", prefix, suffix)
}

fn trim_trailing_slash(value: impl AsRef<str>) -> String {
    value.as_ref().trim().trim_end_matches('/').to_string()
}

fn local_api_text_response(status: u16, text: &str) -> Value {
    json!({
        "status": status,
        "ok": status < 400,
        "headers": {
            "content-type": "text/plain; charset=utf-8"
        },
        "body": text
    })
}

fn string_or_null(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_str)
        .map(|text| Value::String(text.to_string()))
        .unwrap_or(Value::Null)
}

fn number_or_zero(value: Option<&Value>) -> u64 {
    number_or_option(value).unwrap_or(0.0).max(0.0).trunc() as u64
}

fn number_or_zero_i64(value: Option<&Value>) -> i64 {
    number_or_option(value).unwrap_or(0.0).max(0.0).trunc() as i64
}

fn value_u32(value: Option<&Value>, fallback: u32) -> u32 {
    number_or_option(value)
        .map(|value| value.max(0.0).round() as u32)
        .unwrap_or(fallback)
}

fn value_i32(value: Option<&Value>, fallback: i32) -> i32 {
    number_or_option(value)
        .map(|value| value.round() as i32)
        .unwrap_or(fallback)
}

fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    if max < min {
        return min;
    }
    value.max(min).min(max)
}

fn number_or_option(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str().and_then(|text| text.parse::<f64>().ok())
}

fn first_number_i64(value: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| number_or_option(value.get(*key)))
        .unwrap_or(0.0)
        .max(0.0)
        .trunc() as i64
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn parse_newapi_other(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(_)) => value.cloned().unwrap_or(Value::Null),
        Some(Value::String(text)) if !text.trim().is_empty() => {
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({}))
        }
        _ => json!({}),
    }
}

fn fnv1a_hex(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

fn unix_now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_utc_date_string() -> String {
    let (year, month, day) = civil_from_days((unix_now_seconds() / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn unix_seconds_to_iso(seconds: f64) -> String {
    let seconds = seconds.max(0.0).trunc() as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn parse_iso_timestamp_seconds(value: &str) -> Option<i64> {
    if value.len() < 19 {
        return None;
    }
    let year = value.get(0..4)?.parse::<i64>().ok()?;
    let month = value.get(5..7)?.parse::<u32>().ok()?;
    let day = value.get(8..10)?.parse::<u32>().ok()?;
    let hour = value.get(11..13)?.parse::<i64>().ok()?;
    let minute = value.get(14..16)?.parse::<i64>().ok()?;
    let second = value.get(17..19)?.parse::<i64>().ok()?;
    let mut timestamp =
        days_from_civil(year, month, day)? * 86_400 + hour * 3_600 + minute * 60 + second;
    if value.len() >= 25 {
        let sign = value.as_bytes()[19] as char;
        if sign == '+' || sign == '-' {
            let offset_hour = value.get(20..22)?.parse::<i64>().ok()?;
            let offset_minute = value.get(23..25)?.parse::<i64>().ok()?;
            let offset = offset_hour * 3_600 + offset_minute * 60;
            if sign == '+' {
                timestamp -= offset;
            } else {
                timestamp += offset;
            }
        }
    }
    Some(timestamp)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    }
    .div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month_prime + 2).div_euclid(5) + day - 1;
    let doe = yoe * 365 + yoe.div_euclid(4) - yoe.div_euclid(100) + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn local_api_newapi_summary() -> Value {
    match get_newapi_log_summary() {
        Ok(summary) => summary,
        Err(error) => json!({
            "ok": false,
            "message": error,
            "today": empty_usage_log(),
            "all": empty_usage_log(),
            "coverage": {
                "complete": false
            },
            "sync": Value::Null
        }),
    }
}

fn get_newapi_log_summary() -> Result<Value, String> {
    let database_path = newapi_database_path();
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(&database_path).map_err(|error| error.to_string())?;
    init_newapi_database(&connection)?;
    let (day_start, day_end) = local_day_window_seconds();
    let sync = get_latest_newapi_sync_snapshot(&connection)?;

    Ok(json!({
        "ok": true,
        "today": summarize_newapi_rows(&connection, Some((day_start, day_end)))?,
        "all": summarize_newapi_rows(&connection, None)?,
        "latestCreatedAt": query_single_i64(&connection, "select max(created_at) from newapi_logs")?,
        "coverage": get_newapi_log_coverage(&connection, sync.as_ref())?,
        "sync": sync,
        "account": get_latest_newapi_cache_snapshot(&connection, "newapi_account_cache")?,
        "topup": get_latest_newapi_cache_snapshot(&connection, "newapi_topup_cache")?,
        "database": database_path.to_string_lossy().to_string()
    }))
}

fn newapi_database_path() -> PathBuf {
    std::env::var_os("CODEX_QUOTA_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|local| PathBuf::from(local).join("CodexQuotaGlance").join("data"))
        })
        .unwrap_or_else(|| PathBuf::from("data"))
        .join("newapi-usage.sqlite3")
}

fn init_newapi_database(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
            create table if not exists newapi_logs (
              unique_id text primary key,
              provider_log_id text,
              request_id text,
              created_at integer not null,
              token_name text,
              model_name text,
              group_name text,
              input_tokens integer not null default 0,
              cached_input_tokens integer not null default 0,
              output_tokens integer not null default 0,
              total_tokens integer not null default 0,
              raw_used_amount integer not null default 0,
              other_json text
            );
            create index if not exists idx_newapi_logs_created_at on newapi_logs(created_at);
            create table if not exists newapi_sync_state (
              sync_key text primary key,
              base_url text not null,
              api_user text,
              key_fingerprint text,
              latest_created_at integer,
              last_synced_at integer,
              fail_count integer default 0,
              blocked_until integer,
              backfill_until integer,
              backfill_complete integer default 0,
              backfill_warning text
            );
            create table if not exists newapi_account_cache (
              account_key text primary key,
              base_url text not null,
              api_user text,
              token_fingerprint text,
              snapshot_json text not null,
              updated_at integer not null
            );
            create table if not exists newapi_topup_cache (
              topup_key text primary key,
              base_url text not null,
              api_user text,
              token_fingerprint text,
              snapshot_json text not null,
              updated_at integer not null
            );
            "#,
        )
        .map_err(|error| error.to_string())
}

fn summarize_newapi_rows(
    connection: &Connection,
    day_window: Option<(i64, i64)>,
) -> Result<Value, String> {
    let sql = match day_window {
        Some(_) => {
            r#"
            select
              count(*) as request_count,
              coalesce(sum(input_tokens), 0) as input_tokens,
              coalesce(sum(cached_input_tokens), 0) as cached_input_tokens,
              coalesce(sum(output_tokens), 0) as output_tokens,
              coalesce(sum(total_tokens), 0) as total_tokens,
              coalesce(sum(raw_used_amount), 0) as raw_used_amount
            from newapi_logs
            where created_at >= ?1 and created_at < ?2
            "#
        }
        None => {
            r#"
            select
              count(*) as request_count,
              coalesce(sum(input_tokens), 0) as input_tokens,
              coalesce(sum(cached_input_tokens), 0) as cached_input_tokens,
              coalesce(sum(output_tokens), 0) as output_tokens,
              coalesce(sum(total_tokens), 0) as total_tokens,
              coalesce(sum(raw_used_amount), 0) as raw_used_amount
            from newapi_logs
            "#
        }
    };
    let row = if let Some((start, end)) = day_window {
        connection.query_row(sql, params![start, end], newapi_summary_from_row)
    } else {
        connection.query_row(sql, [], newapi_summary_from_row)
    }
    .map_err(|error| error.to_string())?;
    Ok(row)
}

fn newapi_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let request_count: i64 = row.get("request_count")?;
    let input_tokens: i64 = row.get("input_tokens")?;
    let cached_input_tokens: i64 = row.get("cached_input_tokens")?;
    let output_tokens: i64 = row.get("output_tokens")?;
    let total_tokens: i64 = row.get("total_tokens")?;
    let raw_used_amount: i64 = row.get("raw_used_amount")?;
    let cache_hit_rate = if input_tokens > 0 {
        Some((cached_input_tokens as f64 / input_tokens as f64) * 100.0)
    } else {
        None
    };
    Ok(json!({
        "requestCount": request_count,
        "inputTokens": input_tokens,
        "cachedInputTokens": cached_input_tokens,
        "outputTokens": output_tokens,
        "totalTokens": total_tokens,
        "rawUsedAmount": raw_used_amount,
        "usedAmount": raw_used_amount as f64 / QUOTA_UNITS_PER_CNY,
        "cacheHitRate": cache_hit_rate
    }))
}

fn get_latest_newapi_sync_snapshot(connection: &Connection) -> Result<Option<Value>, String> {
    let first_created_at = query_single_i64(connection, "select min(created_at) from newapi_logs")?;
    let row = connection
        .query_row(
            r#"
            select latest_created_at, last_synced_at, fail_count, blocked_until, backfill_until,
                   backfill_complete, backfill_warning
            from newapi_sync_state
            order by last_synced_at desc
            limit 1
            "#,
            [],
            |row| {
                let blocked_until: Option<i64> = row.get("blocked_until")?;
                let backfill_until: Option<i64> = row.get("backfill_until")?;
                let backfill_complete = row.get::<_, i64>("backfill_complete").unwrap_or(0) == 1;
                let backfill_done = backfill_complete
                    || backfill_until
                        .zip(first_created_at)
                        .map(|(backfill, first)| backfill >= first - SYNC_OVERLAP_SECONDS)
                        .unwrap_or(false);
                let mode = if blocked_until.is_some() {
                    "backoff"
                } else if backfill_until.is_some() && !backfill_done {
                    "backfill"
                } else {
                    "incremental"
                };
                Ok(json!({
                    "mode": mode,
                    "latestCreatedAt": row.get::<_, Option<i64>>("latest_created_at")?,
                    "lastSyncedAt": row.get::<_, Option<i64>>("last_synced_at")?,
                    "failCount": row.get::<_, Option<i64>>("fail_count")?,
                    "blockedUntil": blocked_until,
                    "backfillUntil": backfill_until,
                    "backfillComplete": backfill_complete,
                    "backfillWarning": row.get::<_, Option<String>>("backfill_warning")?
                }))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    Ok(row)
}

fn get_newapi_log_coverage(
    connection: &Connection,
    sync_snapshot: Option<&Value>,
) -> Result<Value, String> {
    let first = query_single_i64(connection, "select min(created_at) from newapi_logs")?;
    let latest = query_single_i64(connection, "select max(created_at) from newapi_logs")?;
    let scanned = sync_snapshot
        .and_then(|value| value.get("backfillUntil"))
        .and_then(Value::as_i64);
    let backfill_complete = sync_snapshot
        .and_then(|value| value.get("backfillComplete"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let warning = sync_snapshot
        .and_then(|value| value.get("backfillWarning"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    let complete_boundary = first.or(latest);
    let complete = complete_boundary.is_some()
        && warning.is_none()
        && (backfill_complete
            || first
                .map(|value| value <= INITIAL_SYNC_START + SYNC_OVERLAP_SECONDS)
                .unwrap_or(false)
            || scanned
                .zip(complete_boundary)
                .map(|(scanned, boundary)| scanned >= boundary - SYNC_OVERLAP_SECONDS)
                .unwrap_or(false));
    let scanned_floor = scanned.unwrap_or(INITIAL_SYNC_START);
    let missing_before_seconds = if complete {
        0
    } else {
        (first.or(complete_boundary).unwrap_or(INITIAL_SYNC_START)
            - INITIAL_SYNC_START.max(scanned_floor))
        .max(0)
    };

    Ok(json!({
        "complete": complete,
        "firstCreatedAt": first,
        "expectedStartAt": INITIAL_SYNC_START,
        "scannedThroughAt": scanned,
        "missingBeforeSeconds": missing_before_seconds,
        "warning": warning
    }))
}

fn get_latest_newapi_cache_snapshot(
    connection: &Connection,
    table: &str,
) -> Result<Option<Value>, String> {
    let sql = match table {
        "newapi_account_cache" => {
            "select snapshot_json from newapi_account_cache order by updated_at desc limit 1"
        }
        "newapi_topup_cache" => {
            "select snapshot_json from newapi_topup_cache order by updated_at desc limit 1"
        }
        _ => return Err("不支持的 New API 缓存表".to_string()),
    };
    let snapshot = connection
        .query_row(sql, [], |row| row.get::<_, String>("snapshot_json"))
        .optional()
        .map_err(|error| error.to_string())?
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    Ok(snapshot)
}

fn query_single_i64(connection: &Connection, sql: &str) -> Result<Option<i64>, String> {
    connection
        .query_row(sql, [], |row| row.get::<_, Option<i64>>(0))
        .map_err(|error| error.to_string())
}

fn local_day_window_seconds() -> (i64, i64) {
    let now = Local::now();
    let start = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .earliest()
        .map(|value| value.timestamp())
        .unwrap_or_else(|| {
            let today = current_utc_date_string();
            parse_iso_timestamp_seconds(&format!("{today}T00:00:00+00:00")).unwrap_or(0)
        });
    (start, start + 86_400)
}

fn empty_usage_log() -> Value {
    json!({
        "requestCount": 0,
        "inputTokens": 0,
        "cachedInputTokens": 0,
        "outputTokens": 0,
        "totalTokens": 0,
        "rawUsedAmount": 0,
        "usedAmount": 0
    })
}

fn codex_account_type() -> &'static str {
    if codex_home_dir().join("auth.json").exists() {
        "official_login"
    } else {
        "api"
    }
}

fn codex_home_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn chrono_like_now() -> String {
    // Keep the payload shape compatible without adding a time crate in this migration step.
    format!("{:?}", std::time::SystemTime::now())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            build_tray_menu(app.handle())?;
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle_capsule" => {
                let _ = toggle_capsule_window(app);
            }
            "settings" => {
                let _ = desktop_open_settings(app.clone());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|app, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = toggle_capsule_window(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            local_api_request,
            desktop_drag_start,
            desktop_drag_move,
            desktop_drag_end,
            desktop_hit_test_regions,
            desktop_toast_open,
            desktop_layout_update,
            desktop_detail_layout_update,
            desktop_saved_position,
            desktop_detail_open,
            desktop_update_ready,
            desktop_update_dismiss,
            desktop_update_open_release,
            desktop_update_open_window,
            desktop_update_download,
            desktop_open_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running Codex Quota Glance Tauri app");
}

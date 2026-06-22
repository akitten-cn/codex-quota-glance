use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Window};

const SETTINGS_TITLE: &str = "Codex Quota Glance 设置";
const UPDATE_TITLE: &str = "Codex Quota Glance 更新";
const GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/akitten-cn/codex-quota-glance/releases/latest";

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
    let method = request.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
    let path = local_api_path(&request.url)?;
    let _headers = request.headers.as_ref();
    let body = match (method.as_str(), path.as_str()) {
        ("GET", "/local-api/health") => json!({
            "ok": true,
            "backend": "tauri-local-api"
        }),
        ("GET", "/local-api/update/latest") => local_api_update_latest().await,
        ("GET", "/local-api/codex/status") => local_api_codex_status(),
        ("GET", "/local-api/codex/token/latest") => json!({
            "ok": true,
            "available": false
        }),
        ("GET", "/local-api/codex/token/summary") => json!({
            "ok": true,
            "accountType": codex_account_type(),
            "today": empty_usage_log(),
            "all": empty_usage_log()
        }),
        ("GET", "/local-api/newapi/logs/summary") => local_api_newapi_summary(),
        ("POST", "/local-api/newapi/logs/sync") => json!({
            "ok": true,
            "mode": "tauri-migration",
            "fetched": 0,
            "inserted": 0,
            "message": "Tauri 本地日志同步正在迁移中",
            "summary": local_api_newapi_summary()
        }),
        ("POST", "/local-api/newapi/diagnose") => json!({
            "ok": true,
            "request": {
                "diagnostics": {
                    "mode": "tauri-migration"
                },
                "rawHttpRequest": request.body.unwrap_or_default()
            },
            "response": {
                "success": false,
                "httpStatus": 0,
                "message": "Tauri New API 诊断正在迁移中",
                "dataKeys": []
            }
        }),
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
fn desktop_hit_test_regions(_payload: Value) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_toast_open(_open: bool) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_layout_update(_layout: Value) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_detail_layout_update(_layout: Value) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn desktop_saved_position(_position: Value) -> Result<(), String> {
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
fn desktop_update_open_window(app: AppHandle, payload: Option<UpdateWindowPayload>) -> Result<(), String> {
    let auto_download = payload.and_then(|value| value.auto_download).unwrap_or(false);
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
    let target_dir = std::env::temp_dir().join("CodexQuotaGlance").join("updates");
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
        return Err(format!("下载安装包失败：HTTP {}", response.status().as_u16()));
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
    WebviewWindowBuilder::new(app, "detail", WebviewUrl::App("index.html?view=detail".into()))
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
    WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html?view=settings".into()))
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
        return Ok(url[index..].split('?').next().unwrap_or(&url[index..]).to_string());
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
        let Some((status, needs_human_attention, completed_task)) = codex_activity_update(&event) else {
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
            "agent_message" if contains_plan_choice_signal(payload) => Some(("waiting_for_user", true, false)),
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
    ["function_call", "custom_tool_call", "web_search_call", "patch_apply_begin"].contains(&event_type)
        || ["function_call", "custom_tool_call", "web_search_call", "patch_apply_begin"].contains(&payload_type)
        || ["apply_patch", "exec_command", "write_stdin", "view_image"].contains(&string_field(payload, "name").as_str())
}

fn contains_human_waiting_signal(payload: &Value) -> bool {
    ["type", "name"]
        .iter()
        .map(|key| string_field(payload, key).to_ascii_lowercase())
        .any(|value| value.contains("approval") || value.contains("permission") || value.contains("request_user_input"))
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

fn local_api_newapi_summary() -> Value {
    json!({
        "ok": true,
        "today": empty_usage_log(),
        "all": empty_usage_log(),
        "coverage": {
            "complete": false
        },
        "sync": {
            "mode": "tauri-migration",
            "backfillComplete": false
        }
    })
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

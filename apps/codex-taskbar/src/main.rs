//! Codex Taskbar 进程入口。
//!
//! 默认模式启动原生浮窗、Codex App Server 会话与 SQLite 只读后备。
//! 视觉预览模式仅创建真实任务栏宿主用于 UI 验收，不注入生产数据。

// Release 是任务栏常驻 GUI，不应伴随黑色控制台窗口；Debug 继续保留控制台，
// 方便开发阶段直接观察 panic 与临时命令输出。结构化日志仍写入应用日志目录。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use codex_taskbar::{RunMode, app_data_dir, parse_run_mode, probe_config, redacted_config_summary};
use codex_taskbar_settings::AppConfig;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let [flag, target, pid] = arguments.as_slice()
        && flag == "--apply-update"
    {
        let pid = pid.parse::<u32>().map_err(std::io::Error::other)?;
        codex_taskbar::updater::apply_staged_update(std::path::Path::new(target), pid)?;
        return Ok(());
    }
    if let [flag, staged] = arguments.as_slice()
        && flag == "--cleanup-update"
    {
        codex_taskbar::updater::cleanup_staged_update(std::path::Path::new(staged));
    }
    let mode_arguments =
        if matches!(arguments.as_slice(), [flag, _] if flag == "--cleanup-update") { Vec::new() } else { arguments };
    let mode = parse_run_mode(mode_arguments).map_err(std::io::Error::other)?;
    if mode == RunMode::Help {
        println!(
            "codex-taskbar [--check-config | --probe-plan | --visual-preview | --visual-preview-idle | --visual-preview-weekly-only | --visual-preview-weekly-credits | --visual-preview-details | --visual-preview-details-weekly-only | --visual-preview-details-weekly-credits | --visual-preview-strip | --webview-preview | --settings-preview | --help]"
        );
        return Ok(());
    }

    let local_app_data = std::env::var_os("LOCALAPPDATA");
    // 便携版和受限测试环境可使用专用变量改变本应用数据目录；不复用或篡改
    // LOCALAPPDATA，正式安装默认行为保持不变。
    let root = std::env::var_os("CODEX_TASKBAR_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| app_data_dir(local_app_data.as_deref().map(std::path::Path::new)));
    // 保留这个逻辑路径供 reload marker 和旧 JSON 导入定位；AppConfig 实际以同目录
    // codex-taskbar.db 为主存储，首次启动会自动导入旧 settings.json。
    let settings_path = root.join("settings.json");
    let settings = AppConfig::load_or_create(&settings_path)?;
    let _ = codex_taskbar_diagnostics::prune_old_logs(&root.join("logs"), settings.log_retention_days);
    let (_log_guard, reload_handle) =
        codex_taskbar_diagnostics::init_with_reload(&root.join("logs"), settings.log_level)?;

    tracing::info!(
        event = "application_started",
        schema_version = 3,
        platform_supported = codex_taskbar_platform_windows::is_supported(),
        log_level = %settings.log_level,
        "Codex Taskbar 已启动"
    );

    match mode {
        RunMode::CheckConfig => println!("{}", redacted_config_summary(&settings)),
        RunMode::ProbePlan => print_probe_plan(&settings)?,
        RunMode::Run => codex_taskbar::runtime::run(
            &settings,
            &settings_path,
            &reload_handle,
            std::env::var_os("USERPROFILE").as_deref().map(std::path::Path::new),
            local_app_data.as_deref().map(std::path::Path::new),
        )?,
        RunMode::SettingsPreview => codex_taskbar::runtime::run_settings_preview(&settings_path)?,
        RunMode::VisualPreview => {
            codex_taskbar::runtime::run_visual_preview(&settings, &settings_path, codex_taskbar::PreviewPopup::None)?
        }
        RunMode::VisualPreviewIdle => codex_taskbar::runtime::run_idle_visual_preview(&settings, &settings_path)?,
        RunMode::VisualPreviewWeeklyOnly => {
            codex_taskbar::runtime::run_weekly_only_visual_preview(&settings, &settings_path)?
        }
        RunMode::VisualPreviewWeeklyCredits => {
            codex_taskbar::runtime::run_weekly_credits_visual_preview(&settings, &settings_path)?
        }
        RunMode::VisualPreviewDetails => {
            codex_taskbar::runtime::run_visual_preview(&settings, &settings_path, codex_taskbar::PreviewPopup::Details)?
        }
        RunMode::VisualPreviewDetailsWeeklyOnly => {
            codex_taskbar::runtime::run_weekly_only_details_visual_preview(&settings, &settings_path)?
        }
        RunMode::VisualPreviewDetailsWeeklyCredits => {
            codex_taskbar::runtime::run_weekly_credits_details_visual_preview(&settings, &settings_path)?
        }
        RunMode::VisualPreviewStrip => codex_taskbar::runtime::run_visual_preview(
            &settings,
            &settings_path,
            codex_taskbar::PreviewPopup::TokenStrip,
        )?,
        RunMode::WebViewPreview => codex_taskbar::webview_preview::run()?,
        RunMode::Help => unreachable!(),
    }

    Ok(())
}

#[cfg(windows)]
fn print_probe_plan(settings: &AppConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = codex_taskbar_platform_windows::enable_per_monitor_dpi_awareness();
    let placement = codex_taskbar_platform_windows::discover_probe_placement(&probe_config(settings))?;
    println!(
        "monitor={} taskbar={:?} host_rect=({}, {}, {}, {}) monitor_rect=({}, {}, {}, {}) placement=({}, {}, {}, {}) dpi={} embedded=true webview2=true",
        placement.taskbar.monitor_device,
        placement.taskbar.class,
        placement.taskbar.geometry.taskbar_rect.left,
        placement.taskbar.geometry.taskbar_rect.top,
        placement.taskbar.geometry.taskbar_rect.right,
        placement.taskbar.geometry.taskbar_rect.bottom,
        placement.taskbar.geometry.monitor_rect.left,
        placement.taskbar.geometry.monitor_rect.top,
        placement.taskbar.geometry.monitor_rect.right,
        placement.taskbar.geometry.monitor_rect.bottom,
        placement.rect.left,
        placement.rect.top,
        placement.rect.right,
        placement.rect.bottom,
        placement.taskbar.geometry.dpi
    );
    Ok(())
}

#[cfg(not(windows))]
fn print_probe_plan(_settings: &AppConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err(Box::new(codex_taskbar_platform_windows::PlatformError::UnsupportedPlatform))
}

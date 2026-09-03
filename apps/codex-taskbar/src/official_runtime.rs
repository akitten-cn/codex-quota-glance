//! 官方登录专用运行时。
//!
//! 本模块刻意不依赖 New API、价格表、账本或认证来源选择；常驻进程只连接
//! 官方 Codex app-server，并以只读 SQLite 安全元数据补足桌面任务活动状态。

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use codex_taskbar_adapters_codex_app_server::{
    CodexCliLocator, CodexCliLocatorInput, CodexSession, CodexSessionConfig, SessionFreshness, SessionUpdate,
    SourceHealth,
};
use codex_taskbar_adapters_codex_sqlite::{CodexSqliteFallback, FallbackSnapshot, SqliteFallbackConfig};
use codex_taskbar_application::{
    TokenUsageSnapshot,
    coordinator::{MonitorCoordinator, TelemetryUpdate},
    local_usage_ledger::{LocalUsageClock, LocalUsageLedger, PersistedLocalUsageLedger, ThreadTokenCounter},
    monitor::MonitorState,
    ui_snapshot::{ConsumptionPopupSnapshot, TaskbarSnapshot},
};
use codex_taskbar_domain::{
    activity::ActivityState,
    official::OfficialFreshness,
    usage::{TokenCounts, UsageSource},
};
#[cfg(windows)]
use codex_taskbar_platform_windows::ProbePlacement;
use codex_taskbar_platform_windows::host::{
    NativeDetailRow, NativeHostConfig, NativeHostDetails, NativeHostEvent, NativeHostHandle, NativeNotification,
    NativeNotificationKind, NativeTrendPoint, NativeTrendSeries, TaskbarParent, spawn_native_host,
};
use codex_taskbar_settings::{AppConfig, SyncMode, consume_reload_request, request_reload, settings_database_path};
use codex_taskbar_settings_ui::SettingsAction;

use crate::{
    PreviewPopup, official_api_equivalent_cost_micro_usd, probe_config, session_token_fallback::SessionTokenTailer,
    taskbar_host_details_with_settings, taskbar_host_model, visual_preview_idle_state, visual_preview_state,
    visual_preview_weekly_credits_state, visual_preview_weekly_only_state,
};

const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(any(target_os = "macos", test))]
#[path = "macos_runtime.rs"]
pub mod macos;
const ECONOMY_FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// 聚合账本只按批次写入。即使 SQLite 后备每 5 秒轮询，也不会造成高频磁盘写入。
const LOCAL_LEDGER_FLUSH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LEGACY_LOCAL_LEDGER_FILE_NAME: &str = "local-usage-ledger.json";
const PREVIEW_LOOP_INTERVAL: Duration = Duration::from_millis(250);
/// 旧胶囊版的 session Token 后备每秒最多读取一个活跃 JSONL 的尾部，不扫描正文，
/// 也绝不写入 Codex 目录。
const SESSION_TOKEN_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// App Server 的活动事件只在短时间内优先于桌面 SQLite；自启动的 service 不能
/// 凭一条过期事件永久遮蔽用户正在运行的另一个 Codex 桌面任务。
const APP_SERVER_ACTIVITY_LEASE: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum RuntimeUpdate {
    Codex(Box<SessionUpdate>),
}

/// 自动快览只响应可信的官方 Token 累计正向变化或官方额度的真实下降。
///
/// 首帧只建立基线；旧快照、同值重放、线程切换导致的累计下降以及 SQLite 后备都不能
/// 触发弹窗。桌面 Codex 的 App Server 有时只提供额度刷新、而不提供当前线程 Token，
/// 因而也监听同一官方快照中剩余额度的下降，保持“发生消耗才短暂上浮”的体验。
#[derive(Debug, Default)]
struct ConsumptionPopupTracker {
    latest_token: Option<TokenObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenObservation {
    observed_at_unix_ms: i64,
    total: u64,
}

#[derive(Debug, Default)]
struct AppServerActivityLease {
    expires_at: Option<Instant>,
}

impl AppServerActivityLease {
    fn observe(&mut self, activity_present: bool, source_health: SourceHealth) {
        if matches!(source_health, SourceHealth::Disconnected | SourceHealth::Stopped) {
            self.expires_at = None;
        } else if activity_present {
            self.expires_at = Instant::now().checked_add(APP_SERVER_ACTIVITY_LEASE);
        }
    }

    fn is_live(&self) -> bool {
        self.is_live_at(Instant::now())
    }

    fn is_live_at(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| now < expires_at)
    }
}

impl ConsumptionPopupTracker {
    fn reset(&mut self) {
        self.latest_token = None;
    }

    /// 返回事件时，调用方应刷新已经打开的快览或显示一个新的快览。
    fn observe(&mut self, snapshot: Option<&TokenUsageSnapshot>) -> bool {
        self.observe_token_usage(snapshot)
    }

    fn observe_token_usage(&mut self, snapshot: Option<&TokenUsageSnapshot>) -> bool {
        let Some(snapshot) = snapshot else {
            return false;
        };
        if snapshot.source != UsageSource::AppServer {
            return false;
        }
        let Some(total) = snapshot.current_thread.as_ref().and_then(TokenCounts::display_total) else {
            return false;
        };
        let current = TokenObservation { observed_at_unix_ms: snapshot.observed_at_unix_ms, total };
        let Some(previous) = self.latest_token else {
            self.latest_token = Some(current);
            return false;
        };
        if current.observed_at_unix_ms <= previous.observed_at_unix_ms {
            return false;
        }
        self.latest_token = Some(current);
        current.total > previous.total
    }
}

/// 启动官方登录常驻任务栏。
#[cfg(windows)]
pub fn run(
    settings: &AppConfig,
    settings_path: &Path,
    _log_reload: &codex_taskbar_diagnostics::ReloadHandle,
    profile_dir: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = codex_taskbar_platform_windows::enable_per_monitor_dpi_awareness();
    let placement = codex_taskbar_platform_windows::discover_probe_placement(&probe_config(settings))?;
    let mut active_settings = settings.clone();
    let mut coordinator = MonitorCoordinator::default();
    let host = create_host(&placement, &coordinator, &active_settings)?;
    let mut surface_size = placement_surface_size_dip(&placement);
    let (settings_action_sender, settings_action_receiver) = mpsc::channel::<SettingsAction>();
    if crate::updater::configured_repository().is_some() {
        let automatic_update_sender = settings_action_sender.clone();
        let _ = std::thread::Builder::new().name("codex-taskbar-update-scheduler".to_owned()).spawn(move || {
            // 避开启动期的账户与 WebView 初始化；正式 Release 此后每天检查一次。
            std::thread::sleep(Duration::from_secs(60));
            loop {
                if automatic_update_sender.send(SettingsAction::CheckUpdates).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(24 * 60 * 60));
            }
        });
    }

    let locator_input = CodexCliLocatorInput::new(
        active_settings.codex_cli_path.as_deref().map(PathBuf::from),
        local_app_data.map(Path::to_path_buf),
        codex_path_candidates(std::env::var_os("PATH").as_deref()),
    );
    let (sender, receiver) = mpsc::channel::<RuntimeUpdate>();
    let (session, updates) = match CodexCliLocator::default().locate(&locator_input) {
        Ok(located) => {
            tracing::info!(event = "official_codex_cli_located", executable = %located.safe_summary().file_name);
            let (session, updates) =
                CodexSession::start_process(located.transport_config(), CodexSessionConfig::default());
            (Some(session), updates)
        }
        Err(error) => {
            tracing::warn!(event = "official_codex_cli_unavailable", error = %error, "官方额度暂不可读，继续使用本机状态后备");
            let (_hold, updates) = mpsc::channel();
            // `_hold` 离开作用域后会关闭通道；主循环仍可持续运行 SQLite 后备。
            (None, updates)
        }
    };
    let bridge = sender.clone();
    let _bridge_thread =
        std::thread::Builder::new().name("codex-taskbar-official-bridge".to_owned()).spawn(move || {
            while let Ok(update) = updates.recv() {
                if bridge.send(RuntimeUpdate::Codex(Box::new(update))).is_err() {
                    break;
                }
            }
        })?;
    let fallback = profile_dir.map(|profile| FallbackSources::discover(&profile.join(".codex")));
    let mut session_token_tailer =
        profile_dir.map(|profile| SessionTokenTailer::new(profile.join(".codex").join("sessions")));
    let ledger_path = local_usage_ledger_path(settings_path);
    let mut local_usage_ledger = load_local_usage_ledger(&ledger_path);
    local_usage_ledger.set_retention_days(usize::from(active_settings.history_retention_days));
    let result = run_loop(
        &mut active_settings,
        settings_path,
        _log_reload,
        &host,
        &mut coordinator,
        &mut surface_size,
        &receiver,
        &settings_action_sender,
        &settings_action_receiver,
        session.as_ref(),
        fallback.as_ref(),
        session_token_tailer.as_mut(),
        &mut local_usage_ledger,
        &ledger_path,
    );
    flush_local_usage_ledger(&ledger_path, &mut local_usage_ledger);
    if let Some(session) = session {
        session.stop();
    }
    let _ = host.request_exit();
    result
}

/// 打开主程序内的设置窗口。
///
/// 设置窗口是 `codex-taskbar.exe` 的同进程 UI，不会再启动独立设置软件。
#[cfg(windows)]
pub fn run_settings_preview(settings_path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (actions, _requests) = mpsc::channel();
    launch_settings_window(settings_path, actions)?;
    // 视觉验收模式需要让主线程持续存在；真实运行模式由任务栏消息循环持有进程，
    // 不经过这条预览分支。结束调试进程即可关闭该临时窗口。
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(windows)]
pub fn run_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
    popup: PreviewPopup,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(settings, settings_path, popup, visual_preview_state(now_unix_ms()))
}

#[cfg(windows)]
pub fn run_idle_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(settings, settings_path, PreviewPopup::None, visual_preview_idle_state(now_unix_ms()))
}

/// 启动没有 5 小时额度的固定验收场景。
#[cfg(windows)]
pub fn run_weekly_only_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(
        settings,
        settings_path,
        PreviewPopup::None,
        visual_preview_weekly_only_state(now_unix_ms()),
    )
}

/// 打开“仅 Weekly”的详情卡固定场景，用于验收 5 小时卡自动隐藏后剩余布局。
#[cfg(windows)]
pub fn run_weekly_only_details_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(
        settings,
        settings_path,
        PreviewPopup::Details,
        visual_preview_weekly_only_state(now_unix_ms()),
    )
}

/// 启动 Weekly 耗尽后显示官方 Credits 的固定验收场景。
#[cfg(windows)]
pub fn run_weekly_credits_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(
        settings,
        settings_path,
        PreviewPopup::None,
        visual_preview_weekly_credits_state(now_unix_ms()),
    )
}

/// 打开“Weekly 耗尽 + 官方余额”的详情卡固定场景，验证余额与重置卡条件显示。
#[cfg(windows)]
pub fn run_weekly_credits_details_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_fixed_visual_preview(
        settings,
        settings_path,
        PreviewPopup::Details,
        visual_preview_weekly_credits_state(now_unix_ms()),
    )
}

#[cfg(windows)]
fn run_fixed_visual_preview(
    settings: &AppConfig,
    settings_path: &Path,
    popup: PreviewPopup,
    state: codex_taskbar_application::monitor::MonitorState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = codex_taskbar_platform_windows::enable_per_monitor_dpi_awareness();
    let placement = codex_taskbar_platform_windows::discover_probe_placement(&probe_config(settings))?;
    let coordinator = MonitorCoordinator::new(state);
    let host = create_host(&placement, &coordinator, settings)?;
    let (settings_actions, settings_requests) = mpsc::channel::<SettingsAction>();
    // 视觉预览用独立内存账本覆盖趋势切换与点位交互；不写入用户正式账本，
    // 也不会进入默认运行路径。
    let mut preview_ledger = LocalUsageLedger::default();
    let (preview_day, _) = codex_taskbar_platform_windows::local_usage_clock();
    preview_ledger.replace_session_day(
        preview_day,
        [
            (0, preview_counts(1_200, 800, 430)),
            (4, preview_counts(2_400, 1_600, 780)),
            (8, preview_counts(4_800, 3_100, 1_450)),
            (12, preview_counts(6_100, 4_000, 2_100)),
            (16, preview_counts(7_300, 4_900, 2_600)),
            (20, preview_counts(8_400, 5_800, 3_000)),
        ],
    );
    for (offset, total) in [41_000, 38_000, 52_000, 46_000, 57_000, 49_000, 61_000].into_iter().enumerate() {
        preview_ledger.replace_session_day(
            preview_day - (7 - offset as i32),
            [(12, TokenCounts { total: Some(total), ..TokenCounts::default() })],
        );
    }
    publish(&host, &coordinator, settings, placement_surface_size_dip(&placement), Some(&preview_ledger))?;
    match popup {
        PreviewPopup::None => {}
        PreviewPopup::Details => {
            // 只在视觉验收时等待宿主完成首帧嵌入后投递一次。此前每 250ms
            // 重复打开详情卡，会与失焦关闭路径交替触发，造成“闪一下又重开”。
            std::thread::sleep(Duration::from_millis(350));
            host.show_details()?;
        }
        PreviewPopup::TokenStrip => {
            host.show_token_strip()?;
            host.update_web_token_strip_snapshot(serde_json::to_string(
                &ConsumptionPopupSnapshot::from_monitor_state(coordinator.state()),
            )?)?;
        }
    }
    let update_shutdown = Arc::new(AtomicBool::new(false));
    loop {
        std::thread::sleep(PREVIEW_LOOP_INTERVAL);
        // 视觉验收也使用产品同一条菜单事件链。否则 WebView2 已正确把“退出”
        // 投递到宿主，却没有运行时取走它，表现为右键退出失效。
        let _ = process_settings_actions(
            &settings_requests,
            None,
            &host,
            settings_path,
            None,
            false,
            &update_shutdown,
            None,
            None,
        );
        if !process_host_events(&host, None, settings_path, &settings_actions) {
            return Ok(());
        }
    }
}

#[cfg(windows)]
fn preview_counts(input: u64, cached_input: u64, output: u64) -> TokenCounts {
    TokenCounts {
        input: Some(input),
        cached_input: Some(cached_input),
        output: Some(output),
        total: Some(input.saturating_add(output)),
        ..TokenCounts::default()
    }
}

#[cfg(windows)]
// 此循环的依赖都由 `run` 一次性装配；拆成全局状态会让原本短生命周期的
// session/receiver/host 产生不必要的共享可变性，因此保留显式参数边界。
#[allow(clippy::too_many_arguments)]
fn run_loop(
    settings: &mut AppConfig,
    settings_path: &Path,
    log_reload: &codex_taskbar_diagnostics::ReloadHandle,
    host: &NativeHostHandle,
    coordinator: &mut MonitorCoordinator,
    surface_size: &mut (f32, f32),
    updates: &mpsc::Receiver<RuntimeUpdate>,
    settings_action_sender: &mpsc::Sender<SettingsAction>,
    settings_action_receiver: &mpsc::Receiver<SettingsAction>,
    session: Option<&CodexSession>,
    fallback: Option<&FallbackSources>,
    mut session_token_tailer: Option<&mut SessionTokenTailer>,
    local_usage_ledger: &mut LocalUsageLedger,
    local_usage_ledger_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut authoritative_healthy = false;
    let mut app_server_activity = AppServerActivityLease::default();
    let mut popup_tracker = ConsumptionPopupTracker::default();
    let mut fallback_initial_observation_logged = false;
    let mut last_fallback = Instant::now().checked_sub(fallback_poll_interval(settings)).unwrap_or_else(Instant::now);
    let mut last_session_token_poll =
        Instant::now().checked_sub(SESSION_TOKEN_POLL_INTERVAL).unwrap_or_else(Instant::now);
    let mut session_token_baseline_ready = false;
    let mut session_today_authoritative = false;
    let mut last_ledger_flush = Instant::now();
    let update_shutdown = Arc::new(AtomicBool::new(false));
    publish(host, coordinator, settings, *surface_size, Some(local_usage_ledger))?;
    loop {
        match updates.recv_timeout(Duration::from_millis(250)) {
            Ok(RuntimeUpdate::Codex(update)) => {
                // 仅记录协议通路是否得到各类正式数据，严禁记录账户、额度数值、
                // Token、线程或服务端原文。这样可在用户机器上定位“已连上但卡片
                // 没更新”和“端点本身未返回”两类问题。
                tracing::info!(
                    event = "official_app_server_snapshot",
                    source_health = ?update.source_health,
                    freshness = ?update.freshness,
                    account_live = update.official.account_status.freshness == OfficialFreshness::Live,
                    quota_live = update.official.quota_status.freshness == OfficialFreshness::Live,
                    usage_live = update.official.usage_status.freshness == OfficialFreshness::Live,
                    has_account = update.official.account.is_some(),
                    has_five_hour = update.quota.five_hour.is_some(),
                    has_weekly = update.quota.weekly.is_some(),
                    has_account_usage = update.official.account_usage.is_some(),
                    has_thread_usage = update.usage.is_some(),
                    "已收到 Codex 官方数据快照"
                );
                if update.reset_account_scoped_state {
                    popup_tracker.reset();
                }
                let consumption_event = popup_tracker.observe(update.usage.as_ref());
                // 一轮额度或账户更新通常不包含活动通知；只有仍在租约期内的
                // 明确活动事件才优先于 SQLite，避免一次启动事件永久阻断桌面状态。
                // App Server 的 `Unknown` 只表示它没有提供阶段，不能当作“有活动的
                // 权威状态”来压制只读 SQLite 中真实的 inProgress Turn。否则桌面
                // Codex 正在运行时会长期停在未知色。只有具体活动枚举才获得租约。
                let has_concrete_app_server_activity =
                    update.activity.as_ref().is_some_and(|activity| is_concrete_activity(activity.state));
                app_server_activity.observe(has_concrete_app_server_activity, update.source_health);
                authoritative_healthy = apply_session_update(coordinator, *update);
                publish(host, coordinator, settings, *surface_size, Some(local_usage_ledger))?;
                if consumption_event {
                    // 已先发布最新详情再弹出，确保快览中的数据与任务栏是同一份快照；
                    // 原生层会合并连续增量并重置其自动隐藏计时器。
                    show_consumption_popup(host, coordinator, settings)?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // CLI 不可用或已退出时接收端会立即返回 Disconnected。显式等待，
                // 让 SQLite 后备仍按节流周期工作，避免常驻程序空转占满一个核心。
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        if last_fallback.elapsed() >= fallback_poll_interval(settings) {
            last_fallback = Instant::now();
            if let Some(fallback) = fallback {
                let observation = apply_fallback(
                    coordinator,
                    fallback,
                    authoritative_healthy,
                    app_server_activity.is_live(),
                    local_usage_ledger,
                    session_today_authoritative,
                );
                if !fallback_initial_observation_logged {
                    // 一次性、无敏感字段的运行自检。它只用于证明真正的用户环境
                    // 是否已经读到 SQLite 活动后备，避免每 5 秒写一行日志。
                    tracing::info!(
                        event = "sqlite_fallback_initial_snapshot",
                        state_snapshot_available = observation.state_snapshot_available,
                        history_activity = ?observation.history_activity,
                        history_applied = observation.history_applied,
                        app_server_activity_live = app_server_activity.is_live(),
                        "已完成 Codex 本机活动状态后备初检"
                    );
                    fallback_initial_observation_logged = true;
                }
                publish(host, coordinator, settings, *surface_size, Some(local_usage_ledger))?;
            }
        }
        if last_session_token_poll.elapsed() >= SESSION_TOKEN_POLL_INTERVAL {
            last_session_token_poll = Instant::now();
            if let Some(tailer) = session_token_tailer.as_deref_mut() {
                let (day_key, hour) = codex_taskbar_platform_windows::local_usage_clock();
                if let Some(batch) = tailer.poll(day_key, hour) {
                    if batch.bootstrap && !batch.events.is_empty() {
                        local_usage_ledger.replace_session_day_priced(
                            day_key,
                            batch.events.iter().map(|event| {
                                (
                                    event.local_hour,
                                    event.counts.clone(),
                                    official_api_equivalent_cost_micro_usd(&event.counts, event.model.as_deref())
                                        .map(|(cost, _)| cost),
                                )
                            }),
                        );
                        session_today_authoritative = true;
                    } else if !batch.bootstrap {
                        for event in &batch.events {
                            local_usage_ledger.observe_session_event_priced(
                                LocalUsageClock { day_key, hour: event.local_hour },
                                &event.counts,
                                official_api_equivalent_cost_micro_usd(&event.counts, event.model.as_deref())
                                    .map(|(cost, _)| cost),
                            );
                        }
                    }
                    let Some(event) = batch.events.last() else {
                        session_token_baseline_ready = true;
                        continue;
                    };
                    let has_input = event.counts.input.is_some();
                    let has_cached_input = event.counts.cached_input.is_some();
                    let has_output = event.counts.output.is_some();
                    coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
                        current_thread: None,
                        last_turn: Some(event.counts.clone()),
                        model_context_window: None,
                        today: None,
                        observed_at_unix_ms: now_unix_ms(),
                        source: UsageSource::SessionLogFallback,
                    })));
                    coordinator.apply(TelemetryUpdate::LocalTodayUsage {
                        counts: local_usage_ledger.today_counts(day_key),
                        observed_at_unix_ms: now_unix_ms(),
                    });
                    // 启动时读到的最新事件属于旧存量；只建立基线，不得一启动就
                    // 弹出历史消耗。之后每个新的 token_count 都直接使用旧胶囊版
                    // 已验证的 last_token_usage 明细填充弹窗。
                    if session_token_baseline_ready && !batch.bootstrap {
                        tracing::info!(
                            event = "session_token_popup_triggered",
                            has_input,
                            has_cached_input,
                            has_output,
                            "已从本机 Codex session 捕获新的 Token 消耗"
                        );
                        show_consumption_popup(host, coordinator, settings)?;
                    } else {
                        tracing::info!(
                            event = "session_token_popup_baseline",
                            has_input,
                            has_cached_input,
                            has_output,
                            "已建立本机 Codex session Token 基线"
                        );
                    }
                    session_token_baseline_ready = true;
                    publish(host, coordinator, settings, *surface_size, Some(local_usage_ledger))?;
                }
            }
        }
        if consume_reload_request(settings_path).unwrap_or(false) {
            if let Ok(next) = AppConfig::load(settings_path) {
                log_reload.reload(next.log_level)?;
                if let Some(root) = settings_path.parent() {
                    let _ = codex_taskbar_diagnostics::prune_old_logs(&root.join("logs"), next.log_retention_days);
                }
                if let Ok(placement) = codex_taskbar_platform_windows::discover_probe_placement(&probe_config(&next)) {
                    host.attach_to_taskbar(
                        TaskbarParent {
                            hwnd: placement.taskbar.hwnd,
                            screen_rect: placement.taskbar.geometry.taskbar_rect,
                        },
                        placement.rect,
                    )?;
                    *surface_size = placement_surface_size_dip(&placement);
                }
                local_usage_ledger.set_retention_days(usize::from(next.history_retention_days));
                *settings = next;
                publish(host, coordinator, settings, *surface_size, Some(local_usage_ledger))?;
                tracing::info!(event = "settings_reloaded", "已应用官方登录设置更新");
            }
        }
        if local_usage_ledger.is_dirty() && last_ledger_flush.elapsed() >= LOCAL_LEDGER_FLUSH_INTERVAL {
            flush_local_usage_ledger(local_usage_ledger_path, local_usage_ledger);
            last_ledger_flush = Instant::now();
        }
        if !process_settings_actions(
            settings_action_receiver,
            session,
            host,
            settings_path,
            Some(coordinator.state()),
            authoritative_healthy,
            &update_shutdown,
            Some(local_usage_ledger),
            Some(local_usage_ledger_path),
        ) {
            return Ok(());
        }
        if update_shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        if !process_host_events(host, session, settings_path, settings_action_sender) {
            return Ok(());
        }
    }
}

const fn fallback_poll_interval(settings: &AppConfig) -> Duration {
    match settings.sync_mode {
        SyncMode::Smart => FALLBACK_POLL_INTERVAL,
        SyncMode::Economy => ECONOMY_FALLBACK_POLL_INTERVAL,
    }
}

#[cfg(windows)]
fn show_consumption_popup(
    host: &NativeHostHandle,
    coordinator: &MonitorCoordinator,
    _settings: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 消耗浮窗有独立快照，不能再覆写详情卡的 `details_web_snapshot`。否则详情
    // 打开期间恰好收到 Token 增量时，虽然原生层正确抑制了浮窗，详情内容仍会
    // 被短版数据替换，趋势页签也会像“自动跳回本机历史”一样闪动。
    // popup 会在 token-strip-ready 后从自身副本重投，不会落回全 -- 的静态 DOM。
    host.update_web_token_strip_snapshot(serde_json::to_string(&ConsumptionPopupSnapshot::from_monitor_state(
        coordinator.state(),
    ))?)?;
    host.refresh_token_strip()?;
    Ok(())
}

/// 聚合账本放在应用设置旁边，便于随设置目录整体备份或删除；文件本身不含线程
/// 标识、Prompt、路径、凭据或 SQLite 正文。
fn local_usage_ledger_path(settings_path: &Path) -> PathBuf {
    settings_database_path(settings_path)
        .unwrap_or_else(|| settings_path.parent().unwrap_or_else(|| Path::new(".")).join("codex-taskbar.db"))
}

/// 读取旧账本失败时从空账本恢复，并记录不含路径/内容的错误类别。不能让一个
/// 已损坏的历史文件阻止额度与活动状态监控启动。
fn load_local_usage_ledger(path: &Path) -> LocalUsageLedger {
    let mut ledger = match LocalUsageLedger::load_sqlite(path) {
        Ok(ledger) => ledger,
        Err(_) => {
            tracing::warn!(event = "local_usage_database_read_failed", "本机 Token 数据库暂不可读，已从空账本恢复");
            LocalUsageLedger::default()
        }
    };
    if !ledger.persisted().days.is_empty() {
        return ledger;
    }

    // 首次升级时导入旧 JSON 聚合账本。导入会按 input+output 重算所有具备明细
    // 的日/小时总量，自动清除历史版本可能写入的缓存重复值。
    let legacy = path.parent().unwrap_or_else(|| Path::new(".")).join(LEGACY_LOCAL_LEDGER_FILE_NAME);
    if let Ok(bytes) = std::fs::read(&legacy)
        && let Ok(persisted) = serde_json::from_slice::<PersistedLocalUsageLedger>(&bytes)
        && (1..=LocalUsageLedger::VERSION).contains(&persisted.version)
    {
        ledger = LocalUsageLedger::from_persisted(persisted);
        if ledger.save_sqlite(path).is_ok() {
            tracing::info!(event = "local_usage_ledger_migrated", "旧本机 Token 聚合账本已迁移到 SQLite");
        }
    }
    ledger
}

/// 将聚合日/小时桶写入 SQLite 事务。调用方负责 5 分钟节流；失败时保留 dirty
/// 标记，下次批次或退出时重试。
fn flush_local_usage_ledger(path: &Path, ledger: &mut LocalUsageLedger) {
    if !ledger.is_dirty() {
        return;
    }
    if path.parent().is_some_and(|parent| std::fs::create_dir_all(parent).is_err()) {
        tracing::warn!(event = "local_usage_database_directory_failed", "本机 Token 数据库目录不可用");
        return;
    }
    if ledger.save_sqlite(path).is_err() {
        tracing::warn!(event = "local_usage_database_write_failed", "本机 Token 数据库写入失败，将在下次批次重试");
    }
}

#[cfg(windows)]
// 设置动作同时需要当前运行快照、账本与更新退出标记；显式参数使预览模式能传
// None，避免引入会跨账户残留的全局可变状态。
#[allow(clippy::too_many_arguments)]
fn process_settings_actions(
    actions: &mpsc::Receiver<SettingsAction>,
    session: Option<&CodexSession>,
    host: &NativeHostHandle,
    settings_path: &Path,
    monitor_state: Option<&MonitorState>,
    official_connected: bool,
    update_shutdown: &Arc<AtomicBool>,
    mut ledger: Option<&mut LocalUsageLedger>,
    ledger_path: Option<&Path>,
) -> bool {
    while let Ok(action) = actions.try_recv() {
        match action {
            SettingsAction::ManualRefresh => {
                if let Some(session) = session {
                    session.request_refresh();
                    let _ = host.show_notification(NativeNotification::new(
                        "正在刷新",
                        "已请求一次官方账户与额度检查。",
                        NativeNotificationKind::Info,
                    ));
                } else {
                    let _ = host.show_notification(NativeNotification::new(
                        "官方数据源不可用",
                        "当前未连接 Codex App Server，本机历史采集仍会继续。",
                        NativeNotificationKind::Info,
                    ));
                }
            }
            SettingsAction::ShowHistory => {
                let _ = host.show_details();
            }
            SettingsAction::ClearHistory => {
                if let (Some(ledger), Some(path)) = (ledger.as_deref_mut(), ledger_path) {
                    ledger.clear();
                    flush_local_usage_ledger(path, ledger);
                    let _ = host.show_notification(NativeNotification::new(
                        "本机历史已清理",
                        "只删除了 Codex Taskbar 的聚合 Token 记录。",
                        NativeNotificationKind::Info,
                    ));
                    tracing::info!(event = "local_usage_history_cleared", "用户已清理本机聚合 Token 历史");
                }
            }
            SettingsAction::ExportDiagnostics => {
                match export_diagnostics(settings_path, monitor_state, official_connected) {
                    Ok(path) => {
                        let _ = open_directory(path.parent());
                        let _ = host.show_notification(NativeNotification::new(
                            "诊断包已生成",
                            "已打开 diagnostics 目录；文件不包含账户、Prompt 或线程标识。",
                            NativeNotificationKind::Info,
                        ));
                    }
                    Err(error) => {
                        tracing::warn!(event = "diagnostics_export_failed", error = %error);
                        let _ = host.show_notification(NativeNotification::new(
                            "诊断包生成失败",
                            "请检查应用数据目录是否可写。",
                            NativeNotificationKind::Info,
                        ));
                    }
                }
            }
            SettingsAction::CheckUpdates => {
                let repository = crate::updater::configured_repository();
                let update_host = host.clone();
                let _ = std::thread::Builder::new().name("codex-taskbar-update-check".into()).spawn(move || {
                    match crate::updater::check_latest(repository.as_deref()) {
                        Ok(crate::updater::UpdateStatus::Current) => {
                            let _ = update_host.show_notification(NativeNotification::new(
                                "已是最新版本",
                                concat!("当前版本 v", env!("CARGO_PKG_VERSION")),
                                NativeNotificationKind::Info,
                            ));
                        }
                        Ok(crate::updater::UpdateStatus::Available(release)) => {
                            let _ = update_host.show_notification(NativeNotification::new(
                                "发现新版本",
                                format!("{} 已可用；点击“下载并安装”完成更新。", release.tag),
                                NativeNotificationKind::Info,
                            ));
                        }
                        Err(error) => notify_update_error(&update_host, &error),
                    }
                });
            }
            SettingsAction::DownloadUpdate => {
                let repository = crate::updater::configured_repository();
                let worker_host = host.clone();
                let data_root = settings_path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
                let adaptive_chunk_download =
                    AppConfig::load(settings_path).map(|settings| settings.adaptive_chunk_download).unwrap_or(true);
                let shutdown = Arc::clone(update_shutdown);
                let _ = host.show_notification(NativeNotification::new(
                    "正在检查更新",
                    "联网、下载与校验将在后台完成，不会暂停任务栏动画。",
                    NativeNotificationKind::Info,
                ));
                let _ = std::thread::Builder::new().name("codex-taskbar-update-download".into()).spawn(move || {
                    match crate::updater::check_latest(repository.as_deref()) {
                        Ok(crate::updater::UpdateStatus::Current) => {
                            let _ = worker_host.show_notification(NativeNotification::new(
                                "无需更新",
                                concat!("当前已是 v", env!("CARGO_PKG_VERSION")),
                                NativeNotificationKind::Info,
                            ));
                        }
                        Ok(crate::updater::UpdateStatus::Available(release)) => {
                            let _ = worker_host.show_notification(NativeNotification::new(
                                "正在下载更新",
                                format!("正在通过 Windows 系统代理下载 {}。", release.tag),
                                NativeNotificationKind::Info,
                            ));
                            match crate::updater::download_and_stage(&release, &data_root, adaptive_chunk_download)
                                .and_then(|staged| crate::updater::launch_update_helper(&staged))
                            {
                                Ok(()) => shutdown.store(true, Ordering::Release),
                                Err(error) => notify_update_error(&worker_host, &error),
                            }
                        }
                        Err(error) => notify_update_error(&worker_host, &error),
                    }
                });
            }
        }
    }
    true
}

#[cfg(windows)]
fn notify_update_error(host: &NativeHostHandle, error: &crate::updater::UpdateError) {
    tracing::warn!(event = "update_failed", error = %error);
    let (title, message) = match error {
        crate::updater::UpdateError::RepositoryMissing => {
            ("发布仓库尚未配置", "本地测试包没有 GitHub 仓库坐标；正式 Release 构建会自动写入。".to_owned())
        }
        _ => ("更新失败", error.to_string()),
    };
    let _ = host.show_notification(NativeNotification::new(title, message, NativeNotificationKind::Info));
}

/// 生成单文件脱敏诊断包。日志只统计文件数量与大小，不复制正文，避免未来某条
/// 第三方错误信息意外包含路径或账户标识；排障所需的运行配置使用既有脱敏摘要。
#[cfg(windows)]
fn export_diagnostics(
    settings_path: &Path,
    monitor_state: Option<&MonitorState>,
    official_connected: bool,
) -> std::io::Result<PathBuf> {
    let root = settings_path.parent().unwrap_or_else(|| Path::new("."));
    let diagnostics_dir = root.join("diagnostics");
    std::fs::create_dir_all(&diagnostics_dir)?;
    let settings = AppConfig::load(settings_path).unwrap_or_default();
    let log_dir = root.join("logs");
    let mut log_files = 0_u64;
    let mut log_bytes = 0_u64;
    let mut error_events = 0_u64;
    if let Ok(entries) = std::fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    log_files = log_files.saturating_add(1);
                    log_bytes = log_bytes.saturating_add(metadata.len());
                    if entry.file_name().to_string_lossy().starts_with("codex-taskbar.jsonl") {
                        error_events = error_events.saturating_add(count_error_events(&entry.path()));
                    }
                }
            }
        }
    }
    let generated_at = now_unix_ms();
    let runtime = monitor_state.map(|state| {
        let official = state.official.as_ref();
        serde_json::json!({
            "official_connected": official_connected,
            "activity": format!("{:?}", state.activity),
            "five_hour": {
                "presence": format!("{:?}", state.five_hour.presence),
                "freshness": format!("{:?}", state.five_hour.freshness),
                "source": format!("{:?}", state.five_hour.source)
            },
            "weekly": {
                "presence": format!("{:?}", state.weekly.presence),
                "freshness": format!("{:?}", state.weekly.freshness),
                "source": format!("{:?}", state.weekly.source)
            },
            "token_usage": {
                "source": format!("{:?}", state.token_usage.source),
                "today_source": format!("{:?}", state.token_usage.today_source),
                "fresh": state.token_usage.fresh,
                "has_last_turn": state.token_usage.last_turn.is_some(),
                "has_today": state.token_usage.today.is_some()
            },
            "official_endpoints": {
                "account": official.map(|value| format!("{:?}", value.account_status.freshness)).unwrap_or_else(|| "Unavailable".into()),
                "quota": official.map(|value| format!("{:?}", value.quota_status.freshness)).unwrap_or_else(|| "Unavailable".into()),
                "usage": official.map(|value| format!("{:?}", value.usage_status.freshness)).unwrap_or_else(|| "Unavailable".into())
            }
        })
    });
    let payload = serde_json::json!({
        "schema_version": 1,
        "generated_at_unix_ms": generated_at,
        "application": "Codex Taskbar",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": "windows-x64",
        "config": crate::redacted_config_summary(&settings),
        "local_ledger_present": local_usage_ledger_path(settings_path).is_file(),
        "runtime": runtime,
        "logs": { "file_count": log_files, "total_bytes": log_bytes, "error_event_count": error_events },
        "privacy": "不含账户、Prompt、回复、密钥、完整路径或线程标识"
    });
    let path = diagnostics_dir.join(format!("codex-taskbar-diagnostics-{generated_at}.json"));
    let bytes = serde_json::to_vec_pretty(&payload).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(windows)]
fn count_error_events(path: &Path) -> u64 {
    use std::io::BufRead;

    let Ok(file) = std::fs::File::open(path) else { return 0 };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains("\"level\":\"ERROR\""))
        .count() as u64
}

#[cfg(windows)]
fn process_host_events(
    host: &NativeHostHandle,
    session: Option<&CodexSession>,
    settings_path: &Path,
    settings_actions: &mpsc::Sender<SettingsAction>,
) -> bool {
    loop {
        let event = match host.try_recv_event() {
            Ok(Some(event)) => event,
            Ok(None) => return true,
            Err(error) => {
                tracing::warn!(event = "native_host_event_channel_stopped", error = %error);
                return false;
            }
        };
        match event {
            NativeHostEvent::ShowDetailsRequested => {
                let _ = host.show_details();
            }
            NativeHostEvent::RefreshRequested => {
                if let Some(session) = session {
                    session.request_refresh();
                }
            }
            NativeHostEvent::OpenSettingsRequested => {
                if let Err(error) = launch_settings_window(settings_path, settings_actions.clone()) {
                    let _ = host.show_notification(NativeNotification::new(
                        "无法打开设置",
                        "未找到设置窗口程序，请重新安装完整软件包。",
                        NativeNotificationKind::Info,
                    ));
                    tracing::warn!(event = "settings_window_launch_failed", error = %error);
                }
            }
            NativeHostEvent::ReloadSettingsRequested => {
                let _ = request_reload(settings_path);
            }
            NativeHostEvent::EditSettingsRequested => {
                let _ = launch_settings_window(settings_path, settings_actions.clone());
            }
            NativeHostEvent::OpenConfigDirectoryRequested => {
                let _ = open_directory(settings_path.parent());
            }
            NativeHostEvent::OpenLogDirectoryRequested => {
                let _ = open_directory(settings_path.parent().map(|directory| directory.join("logs")).as_deref());
            }
            NativeHostEvent::ExitRequested => return false,
        }
    }
}

/// 在主程序内启动唯一的 Rust 设置窗口。
///
/// 不再派生 `codex-taskbar-settings.exe` 子进程，避免设置页在任务栏中变成另一个
/// 应用，也确保用户退出任务栏主程序时不会遗留设置窗口。
#[cfg(windows)]
fn launch_settings_window(settings_path: &Path, actions: mpsc::Sender<SettingsAction>) -> Result<(), String> {
    codex_taskbar_settings_ui::launch(settings_path.to_owned(), actions)
}

/// 打开本应用自己的配置或日志目录；不从遥测、详情数据或用户输入构造路径。
#[cfg(windows)]
fn open_directory(directory: Option<&Path>) -> std::io::Result<()> {
    let directory = directory.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "目录不可用"))?;
    std::fs::create_dir_all(directory)?;
    Command::new("explorer.exe").arg(directory).spawn().map(|_| ())
}

const fn is_concrete_activity(state: ActivityState) -> bool {
    !matches!(state, ActivityState::Unknown)
}

fn apply_session_update(coordinator: &mut MonitorCoordinator, update: SessionUpdate) -> bool {
    if update.reset_account_scoped_state {
        coordinator.apply(TelemetryUpdate::ResetAccountScopedState);
    }
    let healthy = matches!(update.source_health, SourceHealth::Healthy | SourceHealth::Degraded)
        && update.freshness == SessionFreshness::Fresh;
    let quota_live = update.official.quota_status.freshness == OfficialFreshness::Live;
    coordinator.apply(TelemetryUpdate::Official(Box::new(update.official)));
    if quota_live {
        coordinator.apply(TelemetryUpdate::RateLimits(update.quota));
    }
    if let Some(activity) = update.activity.filter(|activity| is_concrete_activity(activity.state)) {
        coordinator
            .apply(TelemetryUpdate::Activity { states: vec![activity.state], observed_at_unix_ms: now_unix_ms() });
    }
    if let Some(usage) = update.usage {
        coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(usage)));
    }
    if !healthy {
        coordinator.apply(TelemetryUpdate::AuthoritativeUnavailable { observed_at_unix_ms: now_unix_ms() });
    }
    healthy
}

#[cfg(windows)]
fn create_host(
    placement: &ProbePlacement,
    coordinator: &MonitorCoordinator,
    settings: &AppConfig,
) -> Result<NativeHostHandle, Box<dyn std::error::Error + Send + Sync>> {
    let (width, height) = placement_surface_size_dip(placement);
    let mut model = taskbar_host_model(coordinator.state(), settings, width, height);
    model.reduce_motion = settings.reduce_motion;
    let host = spawn_native_host(
        NativeHostConfig {
            rect: placement.rect,
            taskbar_parent: TaskbarParent {
                hwnd: placement.taskbar.hwnd,
                screen_rect: placement.taskbar.geometry.taskbar_rect,
            },
            initially_visible: true,
        },
        model,
    )?;
    host.update_details(taskbar_host_details_with_settings(coordinator.state(), settings))?;
    host.update_web_taskbar_snapshot(taskbar_snapshot_json(coordinator.state())?)?;
    Ok(host)
}

#[cfg(windows)]
fn placement_surface_size_dip(placement: &ProbePlacement) -> (f32, f32) {
    let dpi_scale = placement.taskbar.geometry.dpi.max(1) as f32 / 96.0;
    (placement.rect.width().max(1) as f32 / dpi_scale, placement.rect.height().max(1) as f32 / dpi_scale)
}

#[cfg(windows)]
fn publish(
    host: &NativeHostHandle,
    coordinator: &MonitorCoordinator,
    settings: &AppConfig,
    surface_size: (f32, f32),
    local_usage_ledger: Option<&LocalUsageLedger>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut model = taskbar_host_model(coordinator.state(), settings, surface_size.0, surface_size.1);
    model.reduce_motion = settings.reduce_motion;
    host.update_model(model)?;
    let mut details = taskbar_host_details_with_settings(coordinator.state(), settings);
    if let Some(ledger) = local_usage_ledger {
        apply_local_history_to_details(&mut details, ledger);
    }
    host.update_details(details)?;
    host.update_web_taskbar_snapshot(taskbar_snapshot_json(coordinator.state())?)?;
    Ok(())
}

/// 将已经聚合完成的本机日桶接到详情卡。该过程不读取原始会话，也不把线程
/// ID 放入 `NativeHostDetails`；没有任何有效日桶时宁可显示 `--` 和空曲线。
fn apply_local_history_to_details(details: &mut NativeHostDetails, ledger: &LocalUsageLedger) {
    let (day_key, current_hour) = codex_taskbar_platform_windows::local_usage_clock();
    let days = ledger.recent_days(14);
    let total = days.iter().fold(0_u64, |sum, day| sum.saturating_add(day.total_tokens));
    let today = ledger.today_hourly_tokens(day_key);
    let today_cost = ledger.today_hourly_estimated_api_cost_micro_usd(day_key);
    let today_points = today
        .iter()
        .enumerate()
        .take(usize::from(current_hour.min(23)) + 1)
        .map(|(hour, value)| NativeTrendPoint::new(format!("{hour:02}:00"), *value))
        .collect::<Vec<_>>();
    let history_points = days
        .iter()
        .map(|day| NativeTrendPoint::new(local_day_label(day.day_key), day.total_tokens))
        .collect::<Vec<_>>();
    let cost_points = today_cost
        .iter()
        .enumerate()
        .take(usize::from(current_hour.min(23)) + 1)
        .map(|(hour, value)| NativeTrendPoint::new(format!("{hour:02}:00"), *value))
        .collect::<Vec<_>>();
    details.trend_series = vec![
        NativeTrendSeries::new(
            "today",
            "今日消耗趋势",
            "Token / 小时 · 本机记录",
            "今日尚未捕获到可靠 Token 事件",
            today_points.clone(),
        ),
        NativeTrendSeries::new(
            "history",
            "本机历史消耗趋势",
            "Token / 日 · 仅此设备",
            "本机历史记录不足",
            history_points.clone(),
        ),
        NativeTrendSeries::new(
            "cost",
            "今日 API 等价金额趋势",
            "USD / 小时 · 官方标准 API 单价估算",
            "尚未捕获到可识别模型与完整 Token 明细",
            cost_points,
        ),
    ];
    if today_points.iter().any(|point| point.value > 0) {
        details.trend_points = today_points;
        details.trend_title = "今日消耗趋势".to_owned();
    }
    if total > 0 {
        details.primary_rows.push(NativeDetailRow::new("本机历史消耗", compact_token_label(total)));
        if details.trend_points.is_empty() {
            details.trend_points = history_points;
            details.trend_title = "本机历史消耗趋势".to_owned();
        }
    }
    if let Some(amount_micro_usd) = ledger.today_estimated_api_cost_micro_usd(day_key) {
        let estimate = codex_taskbar_platform_windows::host::NativeApiCostEstimate {
            amount_micro_usd,
            model: None,
            source: codex_taskbar_platform_windows::host::NativeApiCostSource::Equivalent,
        };
        details.secondary_rows.push(NativeDetailRow::new("今日 API 等价估算", estimate.display_value()));
        if details.api_cost_estimate.is_none() {
            details.api_cost_estimate = Some(estimate);
        }
        details.footer =
            "金额按每次 session 记录的模型与 OpenAI 官方 API 标准文本单价估算；不是 ChatGPT/Codex 订阅账单。"
                .to_owned();
    }
}

/// 详情卡趋势与任务栏使用同一套 K/M/B 进位规则，但保持此运行时模块自包含，
/// 避免平台层依赖应用 UI 的私有格式化实现。
fn compact_token_label(value: u64) -> String {
    match value {
        value if value >= 1_000_000_000 => format!("{:.2}B", value as f64 / 1_000_000_000.0),
        value if value >= 1_000_000 => format!("{:.1}M", value as f64 / 1_000_000.0),
        value if value >= 1_000 => format!("{:.1}K", value as f64 / 1_000.0),
        value => value.to_string(),
    }
}

/// `YYYYMMDD` 是账本的本地日期键；详情页仅展示 `MM/DD`，不暴露路径或会话
/// 标识。异常键仍安全退回为短横线。
fn local_day_label(day_key: i32) -> String {
    let month = (day_key / 100) % 100;
    let day = day_key % 100;
    if (1..=12).contains(&month) && (1..=31).contains(&day) { format!("{month:02}/{day:02}") } else { "--".to_owned() }
}

/// 任务栏 WebView2 只能接收应用层构造的脱敏展示协议。序列化失败意味着内部
/// 协议不一致，应当让本轮发布失败并留在上一帧，而不能退回原始 SQLite/App Server
/// 数据或把未验证对象拼成页面脚本。
fn taskbar_snapshot_json(state: &MonitorState) -> Result<String, serde_json::Error> {
    serde_json::to_string(&TaskbarSnapshot::from_monitor_state(state))
}

struct FallbackSources {
    state: CodexSqliteFallback,
    history: CodexSqliteFallback,
}

/// 一次 SQLite 后备轮询的脱敏结果。仅用于启动后的一条诊断日志，绝不包含
/// 线程标识、Token、路径、提示词或数据库正文。
#[derive(Debug, Clone, Copy)]
struct FallbackObservation {
    state_snapshot_available: bool,
    history_activity: Option<ActivityState>,
    history_applied: bool,
}

impl FallbackSources {
    fn discover(codex_home: &Path) -> Self {
        Self {
            state: CodexSqliteFallback::new(SqliteFallbackConfig::new(newest_versioned_db(codex_home, "state_"))),
            history: CodexSqliteFallback::new(SqliteFallbackConfig::new(newest_versioned_db(
                codex_home,
                "thread_history_",
            ))),
        }
    }
}

fn apply_fallback(
    coordinator: &mut MonitorCoordinator,
    sources: &FallbackSources,
    authoritative_healthy: bool,
    has_app_server_activity: bool,
    local_usage_ledger: &mut LocalUsageLedger,
    session_today_authoritative: bool,
) -> FallbackObservation {
    let state = sources.state.read_snapshot();
    let state_snapshot_available = state.snapshot.is_some();
    if !session_today_authoritative && let Some(snapshot) = state.snapshot.as_ref() {
        apply_local_today_usage(coordinator, local_usage_ledger, snapshot);
    }
    let has_live_usage = authoritative_healthy
        && coordinator.state().token_usage.current_thread.as_ref().and_then(TokenCounts::display_total).is_some();
    if !has_live_usage {
        if let Some(snapshot) = state.snapshot.as_ref() {
            apply_fallback_tokens(coordinator, snapshot);
        }
    }
    let history = sources.history.read_snapshot();
    let history_activity = history.snapshot.as_ref().map(|snapshot| snapshot.activity);
    let mut history_applied = false;
    if !has_app_server_activity {
        if let Some(snapshot) = history.snapshot {
            let previous_activity = coordinator.state().activity;
            coordinator.apply(TelemetryUpdate::Activity {
                states: vec![snapshot.activity],
                observed_at_unix_ms: snapshot.observed_at_unix_ms,
            });
            if coordinator.state().activity != previous_activity {
                // 仅记录已归一化的活动枚举，用于验证 SQLite 后备是否生效；严禁
                // 记录线程 ID、Turn ID、提示词、工具参数或任何 item_json 内容。
                // 这类切换在工具执行期间可能每数秒发生。仅在用户主动选择 Debug
                // 日志时保留诊断，默认 Info 级别不应因正常 SQLite 轮询持续写盘。
                tracing::debug!(
                    event = "sqlite_activity_fallback_applied",
                    activity = ?coordinator.state().activity,
                    "已应用 Codex 本机活动状态后备"
                );
            }
            history_applied = true;
        }
    }
    FallbackObservation { state_snapshot_available, history_activity, history_applied }
}

fn apply_local_today_usage(
    coordinator: &mut MonitorCoordinator,
    ledger: &mut LocalUsageLedger,
    snapshot: &FallbackSnapshot,
) {
    let (day_key, hour) = codex_taskbar_platform_windows::local_usage_clock();
    let changed = ledger.observe(
        LocalUsageClock { day_key, hour },
        snapshot
            .thread_token_totals
            .iter()
            .map(|total| ThreadTokenCounter { thread_id: total.thread_id.clone(), tokens_used: total.tokens_used }),
    );
    // 首帧没有正向增量时也同步一次 `None`，确保旧窗口不会将上一次运行的
    // 本机数值误当作今天。后续只在累计变化时发出 UI 更新，避免 5 秒轮询抖动。
    if changed || coordinator.state().token_usage.today_source == UsageSource::None {
        coordinator.apply(TelemetryUpdate::LocalTodayUsage {
            counts: ledger.today_counts(day_key),
            observed_at_unix_ms: snapshot.observed_at_unix_ms,
        });
    }
}

fn apply_fallback_tokens(coordinator: &mut MonitorCoordinator, snapshot: &FallbackSnapshot) {
    let total = snapshot.raw_thread_tokens_used.or(snapshot.total_tokens).or_else(|| {
        match (snapshot.input_tokens, snapshot.output_tokens) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        }
    });
    if total.is_none()
        && snapshot.input_tokens.is_none()
        && snapshot.output_tokens.is_none()
        && snapshot.cached_input_tokens.is_none()
    {
        return;
    }
    coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
        current_thread: Some(TokenCounts {
            input: snapshot.input_tokens,
            cached_input: snapshot.cached_input_tokens,
            output: snapshot.output_tokens,
            total,
            ..TokenCounts::default()
        }),
        last_turn: None,
        model_context_window: None,
        today: None,
        observed_at_unix_ms: snapshot.observed_at_unix_ms,
        source: UsageSource::SqliteFallback,
    })));
}

fn codex_path_candidates(path: Option<&OsStr>) -> Vec<PathBuf> {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.join(if cfg!(windows) { "codex.exe" } else { "codex" }))
        .collect()
}

fn newest_versioned_db(directory: &Path, prefix: &str) -> PathBuf {
    let mut candidates = std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name.strip_prefix(prefix)?.strip_suffix(".sqlite")?.parse::<u32>().ok()?;
            Some((version, entry.path()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(version, _)| *version);
    candidates.pop().map(|(_, path)| path).unwrap_or_else(|| directory.join(format!("{prefix}1.sqlite")))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total: u64, observed_at_unix_ms: i64, source: UsageSource) -> TokenUsageSnapshot {
        TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(total), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: None,
            today: None,
            observed_at_unix_ms,
            source,
        }
    }

    #[test]
    fn automatic_preview_only_follows_newer_official_positive_token_deltas() {
        let mut tracker = ConsumptionPopupTracker::default();

        // 初始读取只是基线，不能在程序启动时突然弹出旧用量。
        assert!(!tracker.observe(Some(&usage(100, 10, UsageSource::AppServer))));
        assert!(!tracker.observe(Some(&usage(100, 11, UsageSource::AppServer))));
        assert!(tracker.observe(Some(&usage(120, 12, UsageSource::AppServer))));
        // 线程切换或服务端归零会下降；建立新基线但不误报“新消耗”。
        assert!(!tracker.observe(Some(&usage(8, 13, UsageSource::AppServer))));
        assert!(tracker.observe(Some(&usage(9, 14, UsageSource::AppServer))));
        // 乱序旧包不能回滚基线、更不能再次弹窗。
        assert!(!tracker.observe(Some(&usage(99, 12, UsageSource::AppServer))));
    }

    #[test]
    fn automatic_preview_ignores_sqlite_fallback_and_resets_baseline_on_account_change() {
        let mut tracker = ConsumptionPopupTracker::default();

        assert!(!tracker.observe(Some(&usage(100, 10, UsageSource::SqliteFallback))));
        assert!(!tracker.observe(Some(&usage(100, 11, UsageSource::AppServer))));
        tracker.reset();
        assert!(!tracker.observe(Some(&usage(130, 12, UsageSource::AppServer))));
    }

    #[test]
    fn automatic_preview_never_follows_quota_only_updates() {
        let mut tracker = ConsumptionPopupTracker::default();
        assert!(!tracker.observe(None));
        assert!(!tracker.observe(None));
    }

    #[test]
    fn app_server_activity_lease_expires_instead_of_blocking_desktop_fallback_forever() {
        let expired = AppServerActivityLease { expires_at: Instant::now().checked_sub(Duration::from_millis(1)) };
        assert!(!expired.is_live());
        let lease = AppServerActivityLease { expires_at: Instant::now().checked_add(APP_SERVER_ACTIVITY_LEASE) };
        // 只验证时间语义；SourceHealth 的具体连接状态仍由 session 测试覆盖。
        assert!(lease.is_live());
        assert!(!lease.is_live_at(Instant::now() + APP_SERVER_ACTIVITY_LEASE + Duration::from_millis(1)));
    }

    #[test]
    fn unknown_app_server_event_cannot_block_sqlite_activity_fallback() {
        assert!(!is_concrete_activity(ActivityState::Unknown));
        assert!(is_concrete_activity(ActivityState::Thinking));
        assert!(is_concrete_activity(ActivityState::Idle));
    }

    #[cfg(windows)]
    #[test]
    fn local_usage_ledger_is_replaced_atomically_and_never_serializes_thread_identifiers() {
        let temporary_root =
            std::env::temp_dir().join(format!("codex-taskbar-ledger-test-{}-{}", std::process::id(), now_unix_ms()));
        let path = temporary_root.join("codex-taskbar.db");
        let mut ledger = LocalUsageLedger::default();
        let clock = LocalUsageClock { day_key: 20260829, hour: 11 };
        assert!(
            !ledger
                .observe(clock, [ThreadTokenCounter { thread_id: "private-thread-id".to_owned(), tokens_used: 100 }])
        );
        assert!(
            ledger.observe(clock, [ThreadTokenCounter { thread_id: "private-thread-id".to_owned(), tokens_used: 116 }])
        );
        flush_local_usage_ledger(&path, &mut ledger);
        assert!(!ledger.is_dirty());

        let first_file = std::fs::read(&path).expect("应写入聚合账本");
        assert!(!first_file.windows(b"private-thread-id".len()).any(|bytes| bytes == b"private-thread-id"));

        assert!(
            ledger.observe(clock, [ThreadTokenCounter { thread_id: "private-thread-id".to_owned(), tokens_used: 120 }])
        );
        flush_local_usage_ledger(&path, &mut ledger);
        let restored = load_local_usage_ledger(&path);
        assert_eq!(restored.today_counts(clock.day_key).and_then(|counts| counts.total), Some(20));
        let _ = std::fs::remove_dir_all(&temporary_root);
    }

    #[test]
    fn local_priced_usage_populates_today_estimate_and_cost_trend() {
        let (day_key, hour) = codex_taskbar_platform_windows::local_usage_clock();
        let mut ledger = LocalUsageLedger::default();
        let counts = TokenCounts {
            input: Some(100_000),
            cached_input: Some(40_000),
            output: Some(10_000),
            total: Some(110_000),
            ..TokenCounts::default()
        };
        assert!(ledger.replace_session_day_priced(day_key, [(hour, counts, Some(248_000))]));
        let mut details = NativeHostDetails::default();
        apply_local_history_to_details(&mut details, &ledger);

        assert_eq!(details.api_cost_estimate.as_ref().map(|estimate| estimate.amount_micro_usd), Some(248_000));
        let cost = details.trend_series.iter().find(|series| series.id == "cost").expect("应有金额序列");
        assert!(cost.points.iter().any(|point| point.value == 248_000));
        assert!(details.secondary_rows.iter().any(|row| row.label == "今日 API 等价估算"));
    }
}

#[cfg(not(windows))]
pub fn run(
    _settings: &AppConfig,
    _settings_path: &Path,
    _log_reload: &codex_taskbar_diagnostics::ReloadHandle,
    _profile_dir: Option<&Path>,
    _local_app_data: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err(Box::new(codex_taskbar_platform_windows::PlatformError::UnsupportedPlatform))
}

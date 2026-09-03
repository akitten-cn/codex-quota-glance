//! macOS .app 内的 Rust 数据进程。只通过私有 stdin/stdout 管道传递白名单动作与脱敏快照。
//! 不监听网络端口；窗口、拖拽和 WKWebView 属于同一个 AppKit 主应用。
use super::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn send(value: Value) -> std::io::Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn layout_settings(config: &AppConfig) -> Value {
    json!({"display":if config.prefer_secondary_monitor {"secondary"} else {"primary"},
        "dock":if config.anchor == codex_taskbar_settings::TaskbarAnchor::Left {"left"} else {"right"},
        "width":config.taskbar_width_px,"traffic":config.traffic_monitor_offset_px,"opacity":config.taskbar_background_opacity_percent,
        "width_min":200,"width_max":620,"log_level":config.log_level.as_str(),"reduce_motion":config.reduce_motion,
        "sync_mode":config.sync_mode,"history_retention_days":config.history_retention_days,
        "log_retention_days":config.log_retention_days,"adaptive_chunk_download":config.adaptive_chunk_download})
}

fn updated_settings(current: &AppConfig, input: &Value) -> Result<AppConfig, String> {
    let mut next = current.clone();
    next.prefer_secondary_monitor = match input["display"].as_str() {
        Some("primary") => false,
        Some("secondary") => true,
        _ => return Err("显示器选择无效".into()),
    };
    next.target_monitor_device = None;
    next.anchor = match input["dock"].as_str() {
        Some("left") => codex_taskbar_settings::TaskbarAnchor::Left,
        Some("right") => codex_taskbar_settings::TaskbarAnchor::Right,
        _ => return Err("停靠方向无效".into()),
    };
    let number = |key: &str, min: u64, max: u64| {
        input[key].as_u64().filter(|n| (*n >= min) && (*n <= max)).ok_or_else(|| format!("{key} 超出范围"))
    };
    next.taskbar_width_px = number("width", 200, 620)? as u32;
    next.traffic_monitor_offset_px = number("traffic", 0, 4096)? as i32;
    next.taskbar_background_opacity_percent = number("opacity", 20, 100)? as u8;
    next.log_level = input["log_level"].as_str().ok_or("日志等级缺失")?.parse().map_err(|_| "日志等级无效")?;
    next.sync_mode = match input["sync_mode"].as_str() {
        Some("smart") => SyncMode::Smart,
        Some("economy") => SyncMode::Economy,
        _ => return Err("同步策略无效".into()),
    };
    next.history_retention_days = number("history_retention_days", 30, 365)? as u16;
    next.log_retention_days = number("log_retention_days", 7, 90)? as u16;
    next.reduce_motion = input["reduce_motion"].as_bool().ok_or("动画设置无效")?;
    next.adaptive_chunk_download = input["adaptive_chunk_download"].as_bool().ok_or("下载设置无效")?;
    Ok(next.normalize())
}

fn publish_state(
    coordinator: &MonitorCoordinator,
    settings: &AppConfig,
    ledger: &LocalUsageLedger,
) -> std::io::Result<()> {
    let mut details = taskbar_host_details_with_settings(coordinator.state(), settings);
    apply_local_history_to_details(&mut details, ledger);
    let details: Value =
        serde_json::from_str(&codex_taskbar_platform_windows::web_snapshot::details_web_snapshot(&details))?;
    send(
        json!({"kind":"state","taskbar":TaskbarSnapshot::from_monitor_state(coordinator.state()),"details":details,"settings":layout_settings(settings)}),
    )
}

fn mac_cli_candidates(home: &Path) -> Vec<PathBuf> {
    let mut result = codex_path_candidates(std::env::var_os("PATH").as_deref());
    // Finder 启动的 GUI 环境不继承交互 shell 的 PATH；每个候选仍由原定位器验证 app-server 能力。
    result.extend([
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        home.join("Applications/Codex.app/Contents/Resources/codex"),
        home.join(".local/bin/codex"),
        home.join(".cargo/bin/codex"),
    ]);
    result
}

pub fn run_bridge() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !std::env::args().any(|a| a == "--macos-bridge") {
        return Err("请打开 Codex Taskbar.app，不要直接运行其内部数据进程".into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME 不可用")?;
    let root = std::env::var_os("CODEX_TASKBAR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("Library/Application Support/CodexTaskbar"));
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
    let path = root.join("settings.json");
    let mut settings = AppConfig::load_or_create(&path)?;
    let (_guard, log_reload) = codex_taskbar_diagnostics::init_with_reload(&root.join("logs"), settings.log_level)?;
    let _ = codex_taskbar_diagnostics::prune_old_logs(&root.join("logs"), settings.log_retention_days);
    let ledger_path = local_usage_ledger_path(&path);
    let mut ledger = load_local_usage_ledger(&ledger_path);
    ledger.set_retention_days(usize::from(settings.history_retention_days));
    let mut coordinator = MonitorCoordinator::default();
    publish_state(&coordinator, &settings, &ledger)?;
    let (command_tx, commands) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        // EOF 表示宿主关闭/崩溃，数据进程也应退出并保存，不留下后台孤儿。
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line.len() > 65536 {
                continue;
            }
            if let Ok(v) = serde_json::from_str(&line) {
                if command_tx.send(v).is_err() {
                    return;
                }
            }
        }
        let _ = command_tx.send(json!({"action":"quit"}));
    });
    let smoke = std::env::var_os("CODEX_TASKBAR_SMOKE_TEST").is_some();
    let located = CodexCliLocator::default().locate(&CodexCliLocatorInput::new(
        if smoke { None } else { settings.codex_cli_path.clone().map(PathBuf::from) },
        None,
        if smoke { Vec::new() } else { mac_cli_candidates(&home) },
    ));
    let (session, updates) = match located {
        Ok(cli) => {
            let (s, u) = CodexSession::start_process(cli.transport_config(), CodexSessionConfig::default());
            (Some(s), u)
        }
        Err(_) => {
            send(
                json!({"kind":"health","cli_available":false,"message":"未找到支持 app-server 的 Codex CLI。仍读取本机会话；安装 Codex CLI 后重新打开软件。"}),
            )?;
            let (_, rx) = mpsc::channel();
            (None, rx)
        }
    };
    let mut tailer = SessionTokenTailer::new(codex_home.join("sessions"));
    let mut last_fallback = Instant::now() - FALLBACK_POLL_INTERVAL;
    let mut last_poll = Instant::now() - SESSION_TOKEN_POLL_INTERVAL;
    let mut last_flush = Instant::now();
    let mut last_publish = Instant::now();
    let mut healthy = false;
    let mut lease = AppServerActivityLease::default();
    let mut authoritative_day = None;
    let mut baseline = false;
    let mut popup_tracker = ConsumptionPopupTracker::default();
    let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            let mut dirty = false;
            let mut popup = false;
            while let Ok(command) = commands.try_recv() {
                match command["action"].as_str().unwrap_or("") {
                    "quit" => return Ok(()),
                    "refresh-details" | "manual-refresh" => {
                        if let Some(s) = &session {
                            s.request_refresh();
                        }
                        last_fallback = Instant::now() - ECONOMY_FALLBACK_POLL_INTERVAL;
                        dirty = true;
                    }
                    "save-settings" => {
                        let saved = updated_settings(&settings, &command["settings"]).and_then(|next| {
                            next.save_atomic(&path).map_err(|_| "设置写入失败")?;
                            let read = AppConfig::load(&path).map_err(|_| "设置回读失败")?;
                            if read != next {
                                return Err("设置回读不一致".into());
                            }
                            Ok(read)
                        });
                        match saved {
                            Ok(next) => {
                                settings = next;
                                ledger.set_retention_days(usize::from(settings.history_retention_days));
                                log_reload.reload(settings.log_level)?;
                                send(
                                    json!({"kind":"settings-result","ok":true,"message":"已保存并应用到本机 SQLite"}),
                                )?;
                                dirty = true;
                            }
                            Err(message) => send(json!({"kind":"settings-result","ok":false,"message":message}))?,
                        }
                    }
                    "clear-history" => {
                        ledger.clear();
                        flush_local_usage_ledger(&ledger_path, &mut ledger);
                        coordinator.apply(TelemetryUpdate::LocalTodayUsage {
                            counts: None,
                            observed_at_unix_ms: now_unix_ms(),
                        });
                        dirty = true;
                        send(
                            json!({"kind":"settings-result","ok":true,"message":"已清理本机聚合记录；后续仅记录新的消耗"}),
                        )?;
                    }
                    "export-diagnostics" => {
                        let report = json!({"platform":"macos","version":env!("CARGO_PKG_VERSION"),"cli_available":session.is_some(),"settings":layout_settings(&settings),"ledger_days":ledger.persisted().days.len()});
                        std::fs::write(root.join("diagnostics.json"), serde_json::to_vec_pretty(&report)?)?;
                        send(json!({"kind":"diagnostics-exported"}))?;
                    }
                    "snapshot" => dirty = true,
                    _ => {}
                }
            }
            for update in updates.try_iter() {
                if update.reset_account_scoped_state {
                    popup_tracker.reset();
                }
                popup |= popup_tracker.observe(update.usage.as_ref());
                lease.observe(
                    update.activity.as_ref().is_some_and(|a| is_concrete_activity(a.state)),
                    update.source_health,
                );
                healthy = apply_session_update(&mut coordinator, update);
                dirty = true;
            }
            let (day, hour) = codex_taskbar_platform_windows::local_usage_clock();
            if last_fallback.elapsed() >= fallback_poll_interval(&settings) {
                // 每轮重新定位版本化数据库，支持 Codex 在监视器启动后创建/升级数据库。
                let fallback = FallbackSources::discover(&codex_home);
                apply_fallback(
                    &mut coordinator,
                    &fallback,
                    healthy,
                    lease.is_live(),
                    &mut ledger,
                    authoritative_day == Some(day),
                );
                last_fallback = Instant::now();
                dirty = true;
            }
            if last_poll.elapsed() >= SESSION_TOKEN_POLL_INTERVAL {
                last_poll = Instant::now();
                if let Some(batch) = tailer.poll(day, hour) {
                    if batch.bootstrap && !batch.events.is_empty() {
                        ledger.replace_session_day_priced(
                            day,
                            batch.events.iter().map(|e| {
                                (
                                    e.local_hour,
                                    e.counts.clone(),
                                    official_api_equivalent_cost_micro_usd(&e.counts, e.model.as_deref())
                                        .map(|(c, _)| c),
                                )
                            }),
                        );
                        authoritative_day = Some(day);
                    } else if !batch.bootstrap {
                        for e in &batch.events {
                            ledger.observe_session_event_priced(
                                LocalUsageClock { day_key: day, hour: e.local_hour },
                                &e.counts,
                                official_api_equivalent_cost_micro_usd(&e.counts, e.model.as_deref()).map(|(c, _)| c),
                            );
                        }
                    }
                    if let Some(e) = batch.events.last() {
                        coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
                            current_thread: None,
                            last_turn: Some(e.counts.clone()),
                            model_context_window: None,
                            today: None,
                            observed_at_unix_ms: now_unix_ms(),
                            source: UsageSource::SessionLogFallback,
                        })));
                        popup |= baseline && !batch.bootstrap;
                    }
                    coordinator.apply(TelemetryUpdate::LocalTodayUsage {
                        counts: ledger.today_counts(day),
                        observed_at_unix_ms: now_unix_ms(),
                    });
                    baseline = true;
                    dirty = true;
                }
            }
            if dirty || last_publish.elapsed() >= Duration::from_secs(10) {
                publish_state(&coordinator, &settings, &ledger)?;
                last_publish = Instant::now();
            }
            if popup {
                send(
                    json!({"kind":"popup","snapshot":ConsumptionPopupSnapshot::from_monitor_state(coordinator.state())}),
                )?;
            }
            if last_flush.elapsed() >= Duration::from_secs(30) {
                flush_local_usage_ledger(&ledger_path, &mut ledger);
                last_flush = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })();
    flush_local_usage_ledger(&ledger_path, &mut ledger);
    if let Some(session) = session {
        session.stop();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip_uses_shared_schema_without_private_fields() {
        let original = AppConfig::default();
        let payload = layout_settings(&original);
        assert_eq!(updated_settings(&original, &payload).unwrap(), original.normalize());
        assert!(payload.get("codex_cli_path").is_none());
    }
    #[test]
    fn settings_reject_negative_offset_and_too_small_width() {
        let original = AppConfig::default();
        let mut payload = layout_settings(&original);
        payload["traffic"] = json!(-1);
        assert!(updated_settings(&original, &payload).is_err());
        payload["traffic"] = json!(0);
        payload["width"] = json!(199);
        assert!(updated_settings(&original, &payload).is_err());
    }
    #[test]
    fn mac_gui_searches_bundled_and_homebrew_cli() {
        let paths = mac_cli_candidates(Path::new("/Users/test"));
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin/codex")));
        assert!(paths.contains(&PathBuf::from("/Applications/Codex.app/Contents/Resources/codex")));
    }
}

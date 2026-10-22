use codex_taskbar_diagnostics::{LogLevel, init_with_reload};

#[test]
fn lifecycle_events_reach_jsonl_and_off_filters_without_losing_flush() {
    let root = std::env::temp_dir().join(format!(
        "taskbar-diagnostics-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let (guard, reload) = init_with_reload(&root, "info").unwrap();
    tracing::info!(event = "macos_engine_started", version = "test");
    reload.reload(LogLevel::Off).unwrap();
    tracing::error!(event = "filtered_out");
    reload.reload(LogLevel::Warn).unwrap();
    tracing::warn!(event = "macos_connection_health", code = "disconnected");
    drop(guard);
    let files: Vec<_> = std::fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().path()).collect();
    let contents: String = files.iter().map(|path| std::fs::read_to_string(path).unwrap()).collect();
    let lines: Vec<serde_json::Value> = contents.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["fields"]["event"], "macos_engine_started");
    assert_eq!(lines[1]["fields"]["code"], "disconnected");
    assert!(!contents.contains("filtered_out"));
    for path in files {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir(root);
}

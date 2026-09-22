#[allow(dead_code)]
// 审查复现：临时复制到 apps/codex-taskbar/tests/review_regressions.rs 后运行。
// 全部使用临时目录和合成 Token 事件，不读取用户会话或修改生产账本。
#[path = "../src/session_token_fallback.rs"]
mod session_token_fallback;

use codex_taskbar_application::local_usage_ledger::LocalUsageLedger;
use codex_taskbar_domain::usage::TokenCounts;
use session_token_fallback::SessionTokenTailer;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("taskbar-review-{}-{}", std::process::id(), name));
    fs::create_dir_all(&root).unwrap();
    root
}
fn event(second: u8, input: u64) -> String {
    format!(
        "{}\n",
        serde_json::json!({"timestamp":format!("2026-09-22T04:00:{second:02}Z"),"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":input,"output_tokens":0}}}})
    )
}
fn context(model: &str) -> String {
    format!("{}\n", serde_json::json!({"type":"turn_context","payload":{"model":model}}))
}
fn write(root: &Path, date: &str, name: &str, contents: &str, stamp: u64) -> PathBuf {
    let directory = root.join(date);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(stamp))
        .unwrap();
    path
}
fn sum(batch: Option<session_token_fallback::SessionTokenBatch>) -> u64 {
    batch.into_iter().flat_map(|b| b.events).map(|e| e.counts.display_total().unwrap_or(0)).sum()
}

#[test]
fn review_bootstrap_includes_all_resumed_old_sessions() {
    let root = fixture("old-sessions");
    write(&root, "2026/09/20", "a.jsonl", &event(1, 100), 100);
    write(&root, "2026/09/21", "b.jsonl", &event(2, 200), 200);
    let actual = sum(SessionTokenTailer::new(root).poll(20260922, 12));
    assert_eq!(actual, 300, "两个旧任务今天均有消耗，启动扫描必须覆盖两个文件");
}

#[test]
fn review_old_session_remains_monitored_when_today_folder_exists() {
    let root = fixture("old-incremental");
    write(&root, "2026/09/22", "today.jsonl", &event(1, 100), 100);
    let old = write(&root, "2026/09/21", "old.jsonl", &event(2, 200), 200);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 300);
    fs::OpenOptions::new().append(true).open(old).unwrap().write_all(event(3, 50).as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(3100));
    assert_eq!(sum(tailer.poll(20260922, 12)), 50, "今天目录存在时，仍应追踪昨天创建的活跃任务");
}

#[test]
fn review_parallel_sessions_are_both_collected() {
    let root = fixture("parallel");
    let a = write(&root, "2026/09/22", "a.jsonl", &event(1, 100), 100);
    let b = write(&root, "2026/09/22", "b.jsonl", &event(2, 200), 200);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 300);
    for (path, second, input, stamp) in [(a, 3, 10, 300), (b, 4, 20, 400)] {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(event(second, input).as_bytes()).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(stamp)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(3100));
    assert_eq!(sum(tailer.poll(20260922, 12)), 30, "同一轮询周期不能只记最新文件");
}

#[test]
fn review_tail_model_switch_does_not_recount_old_events() {
    let root = fixture("tail-model");
    let text = format!(
        "{}{}\n{}{}{}",
        context("gpt-5.6-terra"),
        "x".repeat(530_000),
        event(1, 100),
        context("gpt-6-astra"),
        event(2, 200)
    );
    write(&root, "2026/09/22", "a.jsonl", &text, 100);
    let mut tailer = SessionTokenTailer::new(root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 300);
    assert_eq!(sum(tailer.poll(20260922, 12)), 0, "未追加任何日志，不得产生新的计费事件");
}

#[test]
fn review_models_are_not_inherited_across_files() {
    let root = fixture("model-isolation");
    write(&root, "2026/09/22", "a.jsonl", &(context("gpt-6-astra") + &event(1, 100)), 100);
    write(&root, "2026/09/22", "b.jsonl", &event(2, 200), 200);
    let batch = SessionTokenTailer::new(root).poll(20260922, 12).unwrap();
    let unknown = batch.events.iter().find(|e| e.counts.input == Some(200)).unwrap();
    assert_eq!(unknown.model, None, "缺模型的任务不能沿用另一个任务的单价");
}

#[test]
fn review_retention_removes_expired_calendar_days() {
    let mut ledger = LocalUsageLedger::default();
    for day in [20200101, 20260922] {
        ledger.replace_session_day(day, [(12, TokenCounts { total: Some(100), ..Default::default() })]);
    }
    ledger.set_retention_days(30);
    assert_eq!(ledger.recent_days(365).len(), 1, "保留30天不能保留六年前的记录");
}

#[test]
fn review_reconnected_quota_generation_is_accepted() {
    use codex_taskbar_application::{
        RateLimitSnapshot,
        coordinator::{MonitorCoordinator, TelemetryUpdate},
    };
    use codex_taskbar_domain::quota::QuotaValue;
    let mut coordinator = MonitorCoordinator::default();
    let snapshot = |revision, timestamp, used| RateLimitSnapshot {
        five_hour: None,
        weekly: Some(QuotaValue::from_used_percent(used, Some(10080), None)),
        observed_at_unix_ms: timestamp,
        revision,
    };
    coordinator.apply(TelemetryUpdate::RateLimits(snapshot(100, 1000, 20.0)));
    coordinator.apply(TelemetryUpdate::AuthoritativeUnavailable { observed_at_unix_ms: 2000 });
    // session::run_transport 每次重连重新从 revision=0 开始。
    assert!(
        coordinator.apply(TelemetryUpdate::RateLimits(snapshot(1, 3000, 30.0))),
        "重连后更新的额度不应被上一连接的revision拦截"
    );
}

#[test]
fn review_partial_line_large_append_and_identical_events_are_preserved() {
    let root = fixture("partial-large");
    let path = write(&root, "2026/09/22", "a.jsonl", &context("gpt-6-astra"), 100);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 0);
    let text = event(1, 100);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&text.as_bytes()[..text.len() / 2]).unwrap();
    assert_eq!(sum(tailer.poll(20260922, 12)), 0);
    file.write_all(&text.as_bytes()[text.len() / 2..]).unwrap();
    file.write_all(format!("{}\n", "x".repeat(600_000)).as_bytes()).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    assert_eq!(sum(tailer.poll(20260922, 12)), 200);
    assert_eq!(sum(tailer.poll(20260922, 12)), 0);
}

#[test]
fn review_midnight_keeps_late_previous_day_and_new_day_separate() {
    let root = fixture("midnight");
    let path = write(&root, "2026/09/22", "a.jsonl", &event(1, 100), 100);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 100);
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(event(2, 50).as_bytes()).unwrap();
    file.write_all(event(3, 200).replace("2026-09-22", "2026-09-23").as_bytes()).unwrap();
    let batch = tailer.poll(20260923, 12).unwrap();
    assert!(batch.bootstrap);
    assert_eq!(
        batch
            .events
            .iter()
            .filter(|e| e.local_day_key == 20260922)
            .map(|e| e.counts.display_total().unwrap())
            .sum::<u64>(),
        50
    );
    assert_eq!(
        batch
            .events
            .iter()
            .filter(|e| e.local_day_key == 20260923)
            .map(|e| e.counts.display_total().unwrap())
            .sum::<u64>(),
        200
    );
    assert_eq!(sum(tailer.poll(20260923, 12)), 0);
}

#[test]
fn review_clear_cutoff_survives_restart_without_replaying_old_sessions() {
    let root = fixture("clear-cutoff");
    write(&root, "2026/09/22", "a.jsonl", &(event(1, 100) + &event(3, 200)), 100);
    let mut tailer = SessionTokenTailer::new(&root);
    let cutoff = time::OffsetDateTime::parse("2026-09-22T04:00:02Z", &time::format_description::well_known::Rfc3339)
        .unwrap()
        .unix_timestamp()
        * 1000;
    tailer.set_cleared_at_ms(cutoff);
    assert_eq!(sum(tailer.poll(20260922, 12)), 200);
}

#[cfg(windows)]
#[test]
fn review_locked_file_prevents_partial_baseline_and_recovers_without_loss() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = fixture("locked-scan");
    write(&root, "2026/09/22", "a.jsonl", &event(1, 100), 100);
    let b = write(&root, "2026/09/22", "b.jsonl", &event(2, 200), 200);
    let lock = fs::OpenOptions::new().read(true).share_mode(0).open(b).unwrap();
    let mut tailer = SessionTokenTailer::new(&root);
    assert!(tailer.poll(20260922, 12).is_none());
    drop(lock);
    let batch = tailer.poll(20260922, 12).unwrap();
    assert!(batch.bootstrap);
    assert_eq!(sum(Some(batch)), 300);
}

#[test]
fn review_truncation_rebuilds_baseline_instead_of_stalling_all_files() {
    let root = fixture("truncation");
    let path = write(&root, "2026/09/22", "a.jsonl", &(event(1, 100) + &event(2, 200)), 100);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 300);
    fs::write(path, event(3, 50)).unwrap();
    assert!(tailer.poll(20260922, 12).is_none());
    let batch = tailer.poll(20260922, 12).unwrap();
    assert!(batch.bootstrap);
    assert_eq!(sum(Some(batch)), 50);
}

#[test]
fn review_reconnect_still_rejects_delayed_and_out_of_order_snapshots() {
    use codex_taskbar_application::{
        RateLimitSnapshot,
        coordinator::{MonitorCoordinator, TelemetryUpdate},
    };
    let mut coordinator = MonitorCoordinator::default();
    let snapshot = |revision, timestamp| {
        TelemetryUpdate::RateLimits(RateLimitSnapshot {
            five_hour: None,
            weekly: None,
            observed_at_unix_ms: timestamp,
            revision,
        })
    };
    assert!(coordinator.apply(snapshot(100, 1000)));
    coordinator.apply(TelemetryUpdate::AuthoritativeUnavailable { observed_at_unix_ms: 2000 });
    assert!(!coordinator.apply(snapshot(101, 1500)));
    assert!(coordinator.apply(snapshot(1, 3000)));
    assert!(coordinator.apply(snapshot(2, 3500)));
    assert!(!coordinator.apply(snapshot(1, 4000)));
}

#[cfg(windows)]
#[test]
fn followup_locked_old_file_does_not_stall_healthy_incremental_file() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = fixture("isolate-locked");
    let old = write(&root, "2026/09/22", "old.jsonl", &event(1, 100), 100);
    let active = write(&root, "2026/09/22", "active.jsonl", &event(2, 200), 200);
    let mut tailer = SessionTokenTailer::new(&root);
    assert_eq!(sum(tailer.poll(20260922, 12)), 300);
    let lock = fs::OpenOptions::new().read(true).share_mode(0).open(old).unwrap();
    fs::OpenOptions::new().append(true).open(active).unwrap().write_all(event(3, 60).as_bytes()).unwrap();
    assert_eq!(sum(tailer.poll(20260922, 12)), 60);
    drop(lock);
    assert_eq!(sum(tailer.poll(20260922, 12)), 0);
}

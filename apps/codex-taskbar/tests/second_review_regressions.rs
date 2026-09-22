// 临时复制到 apps/codex-taskbar/tests/second_review_temp.rs 运行。
#![allow(dead_code)]
#[path = "../src/session_token_fallback.rs"]
mod session_token_fallback;
use codex_taskbar_application::local_usage_ledger::LocalUsageLedger;
use codex_taskbar_domain::usage::TokenCounts;
use session_token_fallback::SessionTokenTailer;

fn counts(total: u64) -> TokenCounts {
    TokenCounts { input: Some(total), output: Some(0), ..Default::default() }
}

#[test]
fn second_review_bootstrap_must_honor_known_live_classification() {
    let mut ledger = LocalUsageLedger::default();
    ledger.replace_session_day(20260922, [(12, counts(300))]);
    // 重建范围有历史100，但扫描期间明确捕获了边界外真实新增60。
    ledger.reconcile_session_batch(
        20260922,
        true,
        [(12, counts(100), None, None, true), (12, counts(60), None, None, false)],
    );
    assert_eq!(
        ledger.today_counts(20260922).unwrap().display_total(),
        Some(360),
        "bootstrap只能初始化协调状态，不能把明确的live事件改成historical"
    );
}

#[test]
fn second_review_first_new_event_after_empty_bootstrap_is_not_swallowed() {
    let root = std::env::temp_dir()
        .join(format!("taskbar-second-empty-{}", time::OffsetDateTime::now_utc().unix_timestamp_nanos()));
    std::fs::create_dir_all(&root).unwrap();
    let (day, hour) = codex_taskbar_platform_windows::local_usage_clock();
    let mut ledger = LocalUsageLedger::default();
    ledger.replace_session_day(day, [(hour, counts(300))]);
    let mut tailer = SessionTokenTailer::new(&root);
    let empty = tailer.poll(day, hour).unwrap();
    assert!(empty.bootstrap && empty.events.is_empty());
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let timestamp = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    let line = serde_json::json!({"timestamp":timestamp,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":60,"output_tokens":0}}}});
    let path = root.join("new-session.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let first = tailer.poll(day, hour).unwrap();
    assert!(first.bootstrap);
    assert!(!first.events[0].historical, "该事件在实际启动之后发生，分类本身正确");
    ledger.reconcile_session_batch(
        day,
        first.bootstrap,
        first.events.into_iter().map(|e| (e.local_hour, e.counts, None, e.model, e.historical)),
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
    assert_eq!(ledger.today_counts(day).unwrap().display_total(), Some(360), "真实新增60被吞掉");
}

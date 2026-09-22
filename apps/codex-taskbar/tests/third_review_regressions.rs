// 临时复制到 apps/codex-taskbar/tests/third_review_temp.rs 运行。
#![allow(dead_code)]
#[path = "../src/session_token_fallback.rs"]
mod session_token_fallback;
use codex_taskbar_application::local_usage_ledger::{
    LocalUsageClock, LocalUsageLedger, SessionUsageEvent, ThreadTokenCounter,
};
use session_token_fallback::{SessionTokenBatch, SessionTokenTailer};

fn apply(ledger: &mut LocalUsageLedger, batch: SessionTokenBatch, day: i32) {
    // 与 runtime 的单日路径一致，保留 tailer 的 reset 和每条来源分类/时间。
    ledger.reconcile_session_batch_at(
        day,
        batch.reset,
        batch.events.into_iter().map(|e| SessionUsageEvent {
            hour: e.local_hour,
            counts: e.counts,
            model: e.model,
            cost: None,
            historical: e.historical,
            occurred_at_unix_ms: Some(e.occurred_at_unix_ms),
        }),
    );
}

#[test]
fn third_review_delayed_pre_scan_event_is_not_billed_by_both_sources() {
    let root = std::env::temp_dir()
        .join(format!("taskbar-third-review-{}", time::OffsetDateTime::now_utc().unix_timestamp_nanos()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("session.jsonl");
    std::fs::write(&path, "").unwrap();
    let (day, hour) = codex_taskbar_platform_windows::local_usage_clock();
    let clock = LocalUsageClock { day_key: day, hour };
    let mut ledger = LocalUsageLedger::default();
    let counter = |n| [ThreadTokenCounter { thread_id: "synthetic".into(), tokens_used: n }];
    let now_ms = || (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    ledger.observe_at(clock, counter(0), now_ms());
    // 正常事件先产生时间戳，但写入SQLite/JSONL稍晚，发生时间在扫描开始之前。
    let timestamp = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut tailer = SessionTokenTailer::new(&root);
    let initial = tailer.poll(day, hour).unwrap();
    assert!(initial.reset && initial.events.is_empty());
    apply(&mut ledger, initial, day);
    assert!(ledger.observe_at(clock, counter(60), now_ms()));
    assert_eq!(ledger.today_counts(day).unwrap().display_total(), Some(60));
    let line = serde_json::json!({"timestamp":timestamp,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50,"output_tokens":10}}}});
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let delayed = tailer.poll(day, hour).unwrap();
    assert!(!delayed.reset && delayed.events[0].historical);
    apply(&mut ledger, delayed, day);
    let actual = ledger.today_counts(day).unwrap().display_total();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
    assert_eq!(actual, Some(60), "同一笔60先入SQLite再补齐JSONL，不能变成120");
}

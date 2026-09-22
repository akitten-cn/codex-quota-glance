//! 按文件增量读取 Codex session Token。
//!
//! 启动按流扫描 JSONL，随后仅处理新增完整行；只向外投影 Token、模型和时间。
//! 原始行只短暂用于解析，不记录或持久化 Prompt、线程标题、工具参数或消息正文。

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[cfg(any(target_os = "macos", test))]
use std::io::Read;

use codex_taskbar_domain::usage::TokenCounts;
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

const REDISCOVER_INTERVAL: Duration = Duration::from_secs(3);

/// 一条新出现的、可安全传给 UI 的本次 Token 消耗事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenEvent {
    pub counts: TokenCounts,
    /// 与该次 Token 事件最近的 `turn_context.model`。仅接受短模型标识，绝不
    /// 读取或保留提示词、线程名或消息正文。
    pub model: Option<String>,
    pub local_day_key: i32,
    pub local_hour: u8,
    /// 扫描开始前或截断恢复时间范围内的事件；扫描中新发生的事件仍为 live。
    pub historical: bool,
    pub occurred_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenBatch {
    pub events: Vec<SessionTokenEvent>,
    pub bootstrap: bool,
    /// 实际开始新扫描；空扫描后的首次 live 批次不重复重置账本。
    pub reset: bool,
}

/// 文件游标和模型上下文只在内存保存，进程退出后重新建立基线。
#[derive(Debug)]
pub struct SessionTokenTailer {
    root: PathBuf,
    current_file: Option<PathBuf>,
    current_day: Option<i32>,
    files: BTreeMap<PathBuf, FileCursor>,
    last_discovery: Option<Instant>,
    cleared_at_ms: i64,
    captured_today: bool,
    latest_timestamps: BTreeMap<PathBuf, i128>,
    scan_started_at: i128,
}

#[derive(Debug, Clone, Default)]
struct FileCursor {
    offset: u64,
    model: Option<String>,
    initialized: bool,
    recovery_before: Option<i128>,
}

impl SessionTokenTailer {
    /// Mac 的桌面数据库未提供状态时，仅投影最新会话的事件类型与时间戳。
    /// 不返回/记录 Prompt、消息正文、工具参数或会话标识。
    #[cfg(any(target_os = "macos", test))]
    pub fn activity(&self, now_ms: i64) -> Option<codex_taskbar_domain::activity::ActivityState> {
        let mut file = open_shared_for_read(self.current_file.as_ref()?)?;
        let length = file.metadata().ok()?.len();
        let offset = length.saturating_sub(128 * 1024);
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::new();
        file.take(128 * 1024).read_to_end(&mut bytes).ok()?;
        let text = std::str::from_utf8(&bytes).ok()?;
        let text = if offset > 0 { text.split_once('\n')?.1 } else { text };
        session_activity(text, now_ms)
    }
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            current_file: None,
            current_day: None,
            files: BTreeMap::new(),
            last_discovery: None,
            cleared_at_ms: 0,
            captured_today: false,
            latest_timestamps: BTreeMap::new(),
            scan_started_at: OffsetDateTime::now_utc().unix_timestamp_nanos(),
        }
    }

    pub fn set_cleared_at_ms(&mut self, timestamp: i64) {
        self.cleared_at_ms = timestamp;
    }

    /// 整批成功后才提交游标；失败会重试，禁止将部分扫描当作完整基线。
    pub fn poll(&mut self, day_key: i32, _fallback_hour: u8) -> Option<SessionTokenBatch> {
        self.poll_at(day_key, OffsetDateTime::now_utc().unix_timestamp_nanos())
    }

    fn poll_at(&mut self, day_key: i32, scan_time: i128) -> Option<SessionTokenBatch> {
        let bootstrap = self.current_day != Some(day_key);
        let scan_started_at = if bootstrap { scan_time } else { self.scan_started_at };
        let mut cursors = if bootstrap { BTreeMap::new() } else { self.files.clone() };
        let discover = bootstrap || self.last_discovery.is_none_or(|time| time.elapsed() >= REDISCOVER_INTERVAL);
        if discover {
            for path in session_files(&self.root).ok()? {
                cursors.entry(path).or_default();
            }
        }
        let mut events = Vec::new();
        let mut latest_timestamps = self.latest_timestamps.clone();
        for (path, cursor) in &mut cursors {
            // 已归档/移除的旧文件不影响其他文件；本轮发现后消失仍按失败重试。
            if !path.try_exists().ok()? && self.files.contains_key(path) {
                continue;
            }
            let metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(_) if !bootstrap => continue,
                Err(_) => return None,
            };
            if !cursor.initialized {
                cursor.initialized = true;
                // 后续发现的旧文件属于恢复范围；真正新建任务用事件时间区分。
                cursor.recovery_before = self.latest_timestamps.get(path).copied().or(Some(scan_started_at));
            }
            if metadata.len() == cursor.offset && !bootstrap {
                continue;
            }
            let Some(mut file) = open_shared_for_read(path) else {
                if bootstrap {
                    return None;
                }
                continue;
            };
            if metadata.len() < cursor.offset {
                // 日志被截断后重新建立完整基线，不能用旧偏移漏采或永远卡住。
                self.current_day = None;
                self.files.clear();
                self.captured_today = false;
                self.last_discovery = None;
                return None;
            }
            file.seek(SeekFrom::Start(cursor.offset)).ok()?;
            let mut reader = BufReader::new(file);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                let count = reader.read_until(b'\n', &mut bytes).ok()?;
                if count == 0 || bytes.last() != Some(&b'\n') {
                    break;
                }
                cursor.offset += count as u64;
                if !bytes.windows(13).any(|w| w == b"\"token_count\"")
                    && !bytes.windows(14).any(|w| w == b"\"turn_context\"")
                {
                    continue;
                }
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { continue };
                if let Some(model) = turn_context_model(&value) {
                    cursor.model = Some(model.to_owned());
                    continue;
                }
                let Some(counts) = token_counts(&value) else { continue };
                let Some((local_day_key, local_hour)) = event_local_clock(&value) else { continue };
                let timestamp = value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())?;
                let stamp = timestamp.unix_timestamp_nanos();
                latest_timestamps.entry(path.clone()).and_modify(|known| *known = (*known).max(stamp)).or_insert(stamp);
                let late_previous_day = local_day_key < day_key
                    && self.files.get(path).is_some_and(|previous| cursor.offset > previous.offset);
                if (local_day_key != day_key && !late_previous_day)
                    || timestamp.unix_timestamp_nanos() / 1_000_000 <= i128::from(self.cleared_at_ms)
                {
                    continue;
                }
                let model = token_event_model(&value).map(str::to_owned).or_else(|| cursor.model.clone());
                // 多文件扫描期间新写入的行，即使已在打开文件时的长度内，仍是 live。
                let historical = !late_previous_day && cursor.recovery_before.is_some_and(|cutoff| stamp <= cutoff);
                events.push(SessionTokenEvent {
                    counts,
                    model,
                    local_day_key,
                    local_hour,
                    historical,
                    occurred_at_unix_ms: (stamp / 1_000_000) as i64,
                });
            }
        }
        let baseline_batch = bootstrap || !self.captured_today;
        self.captured_today =
            (!bootstrap && self.captured_today) || events.iter().any(|event| event.local_day_key == day_key);
        self.current_file = newest_from_paths(&cursors.keys().cloned().collect::<Vec<_>>());
        self.scan_started_at = scan_started_at;
        self.files = cursors;
        self.latest_timestamps = latest_timestamps;
        self.current_day = Some(day_key);
        if discover {
            self.last_discovery = Some(Instant::now());
        }
        if bootstrap || !events.is_empty() {
            Some(SessionTokenBatch { events, bootstrap: baseline_batch, reset: bootstrap })
        } else {
            None
        }
    }
}

fn session_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(any(target_os = "macos", test))]
fn session_activity(text: &str, now_ms: i64) -> Option<codex_taskbar_domain::activity::ActivityState> {
    use codex_taskbar_domain::activity::ActivityState as A;
    let mut latest = None;
    for line in text.split_inclusive('\n') {
        // 不处理写到一半的 JSONL，避免尚未完成的行引起状态闪跳。
        if !line.ends_with('\n') || line.len() > 65536 {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let Some(ts) =
            value.get("timestamp").and_then(Value::as_str).and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        else {
            continue;
        };
        let stamp = (ts.unix_timestamp_nanos() / 1_000_000) as i64;
        if stamp > now_ms + 5000 {
            continue;
        }
        let p = &value["payload"];
        let state = match (value["type"].as_str(), p["type"].as_str()) {
            (
                Some("event_msg"),
                Some("task_started" | "turn_started" | "agent_reasoning" | "exec_command_end" | "mcp_tool_call_end"),
            ) => Some(A::Thinking),
            (Some("event_msg"), Some("exec_command_begin" | "mcp_tool_call_begin" | "patch_apply_begin")) => {
                Some(A::Executing)
            }
            (Some("event_msg"), Some("task_complete" | "task_completed" | "turn_complete" | "turn_completed")) => {
                Some(A::Completed)
            }
            (Some("event_msg"), Some("turn_aborted" | "task_aborted")) => Some(A::Idle),
            (Some("event_msg"), Some("task_failed" | "turn_failed")) => Some(A::Failed),
            (
                Some("event_msg"),
                Some("exec_approval_request" | "apply_patch_approval_request" | "request_user_input"),
            ) => Some(A::WaitingForUser),
            (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
                Some(if p["name"].as_str().is_some_and(|n| n.ends_with("request_user_input")) {
                    A::WaitingForUser
                } else {
                    A::Executing
                })
            }
            (Some("response_item"), Some("function_call_output" | "custom_tool_call_output" | "reasoning")) => {
                Some(A::Thinking)
            }
            _ => None,
        };
        if let Some(state) = state {
            latest = Some((stamp, state));
        }
    }
    let (stamp, state) = latest?;
    // 结束事件短暂显示完成；缺少后续心跳时不能永远保持“执行中”。
    Some(if now_ms - stamp > 15 * 60 * 1000 || (state == A::Completed && now_ms - stamp > 10000) {
        A::Idle
    } else {
        state
    })
}

fn newest_from_paths(paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .filter_map(|path| {
            fs::metadata(path).and_then(|metadata| metadata.modified()).ok().map(|modified| (modified, path.clone()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn turn_context_model(value: &Value) -> Option<&str> {
    let payload = value.get("payload")?;
    (value.get("type").and_then(Value::as_str) == Some("turn_context")
        || payload.get("type").and_then(Value::as_str) == Some("turn_context"))
    .then(|| payload.get("model").and_then(Value::as_str))
    .flatten()
    .filter(|model| safe_model_identifier(model))
}

fn token_event_model(value: &Value) -> Option<&str> {
    let payload = value.get("payload")?;
    payload
        .get("info")
        .and_then(|info| info.get("last_token_usage"))
        .and_then(|usage| usage.get("model"))
        .and_then(Value::as_str)
        .filter(|model| safe_model_identifier(model))
}

fn safe_model_identifier(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 80
        && model.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn event_local_clock(value: &Value) -> Option<(i32, u8)> {
    let timestamp = value.get("timestamp").and_then(Value::as_str)?;
    let parsed = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?;
    #[cfg(target_os = "macos")]
    {
        return codex_taskbar_platform_windows::local_usage_clock_at(parsed.unix_timestamp());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let local = parsed.to_offset(offset);
        Some((local.year() * 10_000 + i32::from(u8::from(local.month())) * 100 + i32::from(local.day()), local.hour()))
    }
}

#[cfg(windows)]
fn open_shared_for_read(path: &Path) -> Option<File> {
    use std::os::windows::fs::OpenOptionsExt;

    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    std::fs::OpenOptions::new().read(true).share_mode(0x0000_0007).open(path).ok()
}

#[cfg(not(windows))]
fn open_shared_for_read(path: &Path) -> Option<File> {
    File::open(path).ok()
}

fn token_counts(value: &Value) -> Option<TokenCounts> {
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let usage = payload.get("info").and_then(|info| info.get("last_token_usage"))?;
    let input = token_value(usage, "input_tokens");
    let cached_input = token_value(usage, "cached_input_tokens");
    let cache_write_input =
        token_value(usage, "cache_write_input_tokens").or_else(|| token_value(usage, "cache_creation_input_tokens"));
    let output = token_value(usage, "output_tokens");
    let reasoning_output = token_value(usage, "reasoning_output_tokens");
    let total = token_value(usage, "total_tokens").or_else(|| match (input, output) {
        (Some(input), Some(output)) => Some(input.saturating_add(output)),
        _ => None,
    });
    let counts = TokenCounts { input, cached_input, cache_write_input, output, reasoning_output, total };
    counts.display_total().filter(|total| *total > 0).map(|_| counts)
}

fn token_value(object: &Value, field: &str) -> Option<u64> {
    object.get(field).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn mac_activity_follows_lifecycle_not_token_totals() {
        use codex_taskbar_domain::activity::ActivityState as A;
        let event = |kind: &str| {
            format!(
                "{}\n",
                serde_json::json!({"timestamp":"2026-09-04T00:00:00Z","type":"event_msg","payload":{"type":kind}})
            )
        };
        let now = OffsetDateTime::parse("2026-09-04T00:00:01Z", &Rfc3339).unwrap().unix_timestamp() * 1000;
        assert_eq!(session_activity(&event("task_started"), now), Some(A::Thinking));
        assert_eq!(
            session_activity(&format!("{}{}", event("task_started"), event("exec_command_begin")), now),
            Some(A::Executing)
        );
        assert_eq!(
            session_activity(&format!("{}{}", event("task_started"), event("task_complete")), now),
            Some(A::Completed)
        );
        assert_eq!(session_activity(&event("task_complete"), now + 15000), Some(A::Idle));
        assert_eq!(session_activity(&event("task_started"), now + 16 * 60 * 1000), Some(A::Idle));
        assert_eq!(session_activity(&event("request_user_input"), now), Some(A::WaitingForUser));
        assert_eq!(session_activity(&event("token_count"), now), None);
        assert_eq!(session_activity(event("task_complete").trim_end(), now), None);
    }

    #[test]
    fn parses_only_token_count_and_never_needs_message_text() {
        let line = r#"{"timestamp":"2026-08-30T12:34:56Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1200,"cached_input_tokens":800,"cache_creation_input_tokens":100,"output_tokens":300}}}}"#;
        let value = serde_json::from_str(line).expect("fixture 应是 JSON");
        let counts = token_counts(&value).expect("应解析 Token 数字");
        assert_eq!(counts.input, Some(1200));
        assert_eq!(counts.cached_input, Some(800));
        assert_eq!(counts.cache_write_input, Some(100));
        assert_eq!(counts.output, Some(300));
        assert_eq!(counts.total, Some(1500));
    }

    #[test]
    fn ignores_non_token_and_zero_events() {
        let non_token = serde_json::json!({"payload":{"type":"agent_message","text":"private"}});
        assert!(token_counts(&non_token).is_none());
        let zero = serde_json::json!({"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"output_tokens":0}}}});
        assert!(token_counts(&zero).is_none());
    }

    #[test]
    fn bootstrap_reads_all_current_day_events_and_incremental_poll_deduplicates() {
        let root = std::env::temp_dir().join(format!("codex-taskbar-session-tailer-{}", std::process::id()));
        let folder = root.join("2026").join("08").join("30");
        std::fs::create_dir_all(&folder).expect("应创建临时 session 目录");
        let file = folder.join("rollout-test.jsonl");
        let context = r#"{"timestamp":"2026-08-30T12:30:00Z","type":"turn_context","payload":{"model":"gpt-5.6-terra","private_message":"must never be read"}}"#;
        let first = r#"{"timestamp":"2026-08-30T12:34:56Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"total_tokens":120}}}}"#;
        let second = r#"{"timestamp":"2026-08-30T13:34:56Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":80,"output_tokens":30,"total_tokens":230}}}}"#;
        std::fs::write(&file, format!("{context}\n{first}\n{second}\n")).expect("应写入测试 session");
        let day_key =
            event_local_clock(&serde_json::from_str(first).expect("fixture 应有效")).expect("应解析本地时间").0;
        let mut tailer = SessionTokenTailer::new(&root);
        let bootstrap = tailer.poll(day_key, 8).expect("应返回启动批次");
        assert!(bootstrap.bootstrap);
        assert_eq!(bootstrap.events.len(), 2);
        assert!(bootstrap.events.iter().all(|event| event.model.as_deref() == Some("gpt-5.6-terra")));
        assert!(tailer.poll(day_key, 8).is_none());

        let next_context =
            r#"{"timestamp":"2026-08-30T14:30:00Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#;
        let third = r#"{"timestamp":"2026-08-30T14:34:56Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":40,"total_tokens":340}}}}"#;
        let mut writer = std::fs::OpenOptions::new().append(true).open(&file).expect("应打开测试 session");
        writeln!(writer, "{next_context}\n{third}").expect("应追加新事件");
        let incremental = tailer.poll(day_key, 8).expect("应返回新增批次");
        assert!(!incremental.bootstrap);
        assert_eq!(incremental.events.len(), 1);
        assert_eq!(incremental.events[0].counts.total, Some(340));
        assert_eq!(incremental.events[0].model.as_deref(), Some("gpt-5.6-sol"));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn discovery_classifies_restored_records_and_new_tasks_by_observation_boundary() {
        let root = std::env::temp_dir()
            .join(format!("taskbar-classification-{}", OffsetDateTime::now_utc().unix_timestamp_nanos()));
        fs::create_dir_all(&root).unwrap();
        let line = |second| {
            format!(
                "{}\n",
                serde_json::json!({"timestamp":format!("2026-09-22T04:00:{second:02}Z"), "payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50,"output_tokens":10}}}})
            )
        };
        fs::write(root.join("initial.jsonl"), line(1)).unwrap();
        let mut tailer = SessionTokenTailer::new(&root);
        tailer.poll(20260922, 12).unwrap();
        tailer.scan_started_at =
            OffsetDateTime::parse("2026-09-22T04:00:02Z", &Rfc3339).unwrap().unix_timestamp_nanos();
        tailer.last_discovery = None;
        fs::write(root.join("restored.jsonl"), line(1)).unwrap();
        fs::write(root.join("new.jsonl"), line(3)).unwrap();
        let batch = tailer.poll(20260922, 12).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events.iter().filter(|event| event.historical).count(), 1);
        assert_eq!(batch.events.iter().filter(|event| !event.historical).count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bootstrap_does_not_reclassify_events_written_during_scan_as_history() {
        let root = std::env::temp_dir()
            .join(format!("taskbar-scan-boundary-{}", OffsetDateTime::now_utc().unix_timestamp_nanos()));
        fs::create_dir_all(&root).unwrap();
        let event = |second| {
            format!(
                "{}\n",
                serde_json::json!({"timestamp":format!("2026-09-22T04:00:{second:02}Z"), "payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50,"output_tokens":10}}}})
            )
        };
        // 注入扫描开始时刻，文件打开时已经同时包含旧行和扫描中新写入的行。
        fs::write(root.join("session.jsonl"), format!("{}{}", event(1), event(3))).unwrap();
        let start = OffsetDateTime::parse("2026-09-22T04:00:02Z", &Rfc3339).unwrap().unix_timestamp_nanos();
        let batch = SessionTokenTailer::new(&root).poll_at(20260922, start).unwrap();
        assert!(batch.bootstrap && batch.reset);
        assert_eq!(batch.events.len(), 2);
        assert!(batch.events[0].historical);
        assert!(!batch.events[1].historical);
        fs::remove_dir_all(root).unwrap();
    }
}

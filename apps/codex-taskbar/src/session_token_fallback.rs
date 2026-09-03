//! 旧胶囊版已经验证过的 Codex session Token 后备读取。
//!
//! 此模块只读取当前 session JSONL 尾部新增的 `token_count` 行，并且只投影
//! `last_token_usage` 的数值字段。它绝不读取、记录或持久化 Prompt、线程标题、
//! 线程 ID、工具参数或任意消息正文。

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use codex_taskbar_domain::usage::TokenCounts;
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

const TAIL_READ_BYTES: u64 = 512 * 1024;
// 新 session 文件通常在用户发起下一次任务时创建。这里仅枚举当天目录，开销很小，
// 因此将重新发现限制在 3 秒内，避免新会话开始后弹窗要等待半分钟；没有当天文件时
// 也不会每秒递归扫描整个历史目录。
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenBatch {
    pub events: Vec<SessionTokenEvent>,
    pub bootstrap: bool,
}

/// 只读 session 尾部轮询器。事件指纹仅在内存保存，进程退出后会重新建立基线。
#[derive(Debug)]
pub struct SessionTokenTailer {
    root: PathBuf,
    current_file: Option<PathBuf>,
    current_day: Option<i32>,
    seen_fingerprints: HashSet<u64>,
    last_discovery: Option<Instant>,
    current_model: Option<String>,
}

impl SessionTokenTailer {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            current_file: None,
            current_day: None,
            seen_fingerprints: HashSet::new(),
            last_discovery: None,
            current_model: None,
        }
    }

    /// 返回当前最新 Token 事件；同一事件只返回一次。
    ///
    /// 初次调用同样返回事件，调用方应将其当成基线而不是立即弹窗。
    pub fn poll(&mut self, day_key: i32, fallback_hour: u8) -> Option<SessionTokenBatch> {
        if self.current_day != Some(day_key) {
            self.current_day = Some(day_key);
            self.current_file = None;
            self.seen_fingerprints.clear();
            self.last_discovery = None;
            self.current_model = None;
            let mut files = session_files_for_day(&self.root, day_key);
            if let Some(active) = newest_session_file_recursive(&self.root)
                && !files.contains(&active)
            {
                files.push(active);
            }
            self.current_file = newest_from_paths(&files);
            let mut events = Vec::new();
            for file in files {
                let parsed = read_token_events(&file, false, fallback_hour, self.current_model.as_deref());
                self.current_model = parsed.latest_model;
                for (fingerprint, event) in parsed.events {
                    if event.local_day_key == day_key && self.seen_fingerprints.insert(fingerprint) {
                        events.push(event);
                    }
                }
            }
            self.last_discovery = Some(Instant::now());
            return Some(SessionTokenBatch { events, bootstrap: true });
        }
        let mut scan_full_file = false;
        if self.should_discover() {
            let next = newest_session_file(&self.root, day_key).or_else(|| newest_session_file_recursive(&self.root));
            self.last_discovery = Some(Instant::now());
            if next != self.current_file {
                self.current_file = next;
                self.current_model = None;
                scan_full_file = true;
            }
        }
        let file = self.current_file.as_ref()?;
        let parsed = read_token_events(file, !scan_full_file, fallback_hour, self.current_model.as_deref());
        self.current_model = parsed.latest_model;
        let events = parsed
            .events
            .into_iter()
            .filter_map(|(fingerprint, event)| {
                (event.local_day_key == day_key && self.seen_fingerprints.insert(fingerprint)).then_some(event)
            })
            .collect::<Vec<_>>();
        if events.is_empty() { None } else { Some(SessionTokenBatch { events, bootstrap: false }) }
    }

    fn should_discover(&self) -> bool {
        self.last_discovery.is_none_or(|instant| instant.elapsed() >= REDISCOVER_INTERVAL)
            || self.current_file.as_ref().is_some_and(|path| !path.is_file())
    }
}

fn session_files_for_day(root: &Path, day_key: i32) -> Vec<PathBuf> {
    let year = day_key / 10_000;
    let month = (day_key / 100) % 100;
    let day = day_key % 100;
    let folder = root.join(year.to_string()).join(format!("{month:02}")).join(format!("{day:02}"));
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect::<Vec<_>>();
    files.sort();
    files
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

fn newest_session_file(root: &Path, day_key: i32) -> Option<PathBuf> {
    let year = day_key / 10_000;
    let month = (day_key / 100) % 100;
    let day = day_key % 100;
    let folder = root.join(year.to_string()).join(format!("{month:02}")).join(format!("{day:02}"));
    newest_jsonl_in_directory(&folder)
}

/// session 滚动到新日期或用户时区变化时的保守兜底。只在启动或每 30 秒执行，
/// 平时只读取一个活跃文件的尾部，避免高频全目录扫描。
fn newest_session_file_recursive(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(directory) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() || path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else { continue };
            if newest.as_ref().is_none_or(|(known, _)| modified > *known) {
                newest = Some((modified, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

fn newest_jsonl_in_directory(directory: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else { continue };
        if newest.as_ref().is_none_or(|(known, _)| modified > *known) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

struct ParsedTokenEvents {
    events: Vec<(u64, SessionTokenEvent)>,
    latest_model: Option<String>,
}

fn read_token_events(
    path: &Path,
    tail_only: bool,
    fallback_hour: u8,
    initial_model: Option<&str>,
) -> ParsedTokenEvents {
    // Codex 可能正持有活动 JSONL 的写入句柄。显式允许并行读写/重命名，避免
    // Windows 上因共享模式默认值不同而把“正在执行”错误降级成“暂无 Token”。
    let mut latest_model = initial_model.map(str::to_owned);
    let empty = || ParsedTokenEvents { events: Vec::new(), latest_model: latest_model.clone() };
    let Some(mut file) = open_shared_for_read(path) else { return empty() };
    let Ok(metadata) = file.metadata() else { return empty() };
    let length = metadata.len();
    let offset = if tail_only { length.saturating_sub(TAIL_READ_BYTES) } else { 0 };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return empty();
    }
    let mut bytes = Vec::with_capacity((length - offset) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return empty();
    }
    if offset > 0 {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else { return empty() };
        bytes.drain(..=first_newline);
    }
    let Ok(text) = std::str::from_utf8(&bytes) else { return empty() };
    let mut events = Vec::new();
    for line in text.lines() {
        if !line.contains("\"token_count\"") && !line.contains("\"turn_context\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(model) = turn_context_model(&value) {
            latest_model = Some(model.to_owned());
            continue;
        }
        let Some(counts) = token_counts(&value) else { continue };
        let (local_day_key, local_hour) =
            event_local_clock(&value).unwrap_or_else(|| (current_local_day_key(), fallback_hour.min(23)));
        let model = token_event_model(&value).map(str::to_owned).or_else(|| latest_model.clone());
        events.push((
            event_fingerprint(&value, &counts, model.as_deref()),
            SessionTokenEvent { counts, model, local_day_key, local_hour },
        ));
    }
    ParsedTokenEvents { events, latest_model }
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

fn current_local_day_key() -> i32 {
    #[cfg(target_os = "macos")]
    {
        return codex_taskbar_platform_windows::local_usage_clock().0;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let local = OffsetDateTime::now_utc().to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
        local.year() * 10_000 + i32::from(u8::from(local.month())) * 100 + i32::from(local.day())
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

fn event_fingerprint(value: &Value, counts: &TokenCounts, model: Option<&str>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.get("timestamp").and_then(Value::as_str).unwrap_or_default().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    for number in [
        counts.input,
        counts.cached_input,
        counts.cache_write_input,
        counts.output,
        counts.reasoning_output,
        counts.total,
    ]
    .into_iter()
    .flatten()
    {
        for byte in number.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for byte in model.unwrap_or_default().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
}

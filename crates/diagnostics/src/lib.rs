//! 结构化诊断日志入口。
//!
//! 日志默认写入按天滚动的 JSONL 文件；debug 构建额外输出紧凑的控制台日志。
//! 调用方必须先脱敏，禁止把认证信息、提示词、工具参数正文和完整用户路径
//! 写入任何日志字段。日志字段应优先使用事件名、错误码、耗时和哈希后的标识。

use std::sync::{Arc, RwLock};
use std::{
    path::Path,
    time::{Duration, SystemTime},
};

pub use codex_taskbar_settings::LogLevel;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

/// 异步日志初始化返回的错误类型。
pub type DiagnosticsError = Box<dyn std::error::Error + Send + Sync>;

/// 删除超过保留期的本应用滚动日志。只处理固定前缀的普通文件，拒绝链接和目录。
pub fn prune_old_logs(log_dir: &Path, retention_days: u16) -> std::io::Result<usize> {
    prune_old_logs_at(log_dir, retention_days, SystemTime::now())
}

fn prune_old_logs_at(log_dir: &Path, retention_days: u16, now: SystemTime) -> std::io::Result<usize> {
    let cutoff = now
        .checked_sub(Duration::from_secs(u64::from(retention_days.max(1)) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0;
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("codex-taskbar.jsonl") {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() || metadata.modified().unwrap_or(SystemTime::now()) >= cutoff {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

type ReloadCallback = dyn Fn(LogLevel) -> Result<(), String> + Send + Sync + 'static;

/// 可在线调整日志等级的句柄。
///
/// 句柄只替换过滤层，不重建文件 appender 或全局 subscriber，因此热更新不会
/// 丢失 JSONL 文件。多个线程可以安全地共享同一个句柄。
#[derive(Clone)]
pub struct ReloadHandle {
    callback: Arc<ReloadCallback>,
    current: Arc<RwLock<LogLevel>>,
}

impl std::fmt::Debug for ReloadHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ReloadHandle").field("current", &self.current_level()).finish_non_exhaustive()
    }
}

impl ReloadHandle {
    /// 使用新的等级替换文件和控制台的过滤条件。
    pub fn reload(&self, level: LogLevel) -> Result<(), DiagnosticsError> {
        (self.callback)(level).map_err(std::io::Error::other)?;
        if let Ok(mut current) = self.current.write() {
            *current = level;
        }
        Ok(())
    }

    /// 从可编辑的字符串更新等级，解析规则与设置 crate 相同。
    pub fn reload_str(&self, level: &str) -> Result<(), DiagnosticsError> {
        let level = level.parse::<LogLevel>().map_err(|error| Box::new(error) as DiagnosticsError)?;
        self.reload(level)
    }

    /// 返回最近一次成功设置的等级。
    #[must_use]
    pub fn current_level(&self) -> LogLevel {
        self.current.read().map(|level| *level).unwrap_or(LogLevel::Off)
    }

    /// `reload` 的语义别名，便于设置监听器表达“应用新配置”。
    pub fn set_level(&self, level: LogLevel) -> Result<(), DiagnosticsError> {
        self.reload(level)
    }
}

/// 初始化全局结构化日志并返回异步写入 guard。
///
/// 这是兼容旧装配代码的 API。需要运行时热更新时使用
/// [`init_with_reload`]，并保留返回的 [`ReloadHandle`]。
pub fn init<L>(log_dir: &Path, level: L) -> Result<WorkerGuard, DiagnosticsError>
where
    L: AsRef<str>,
{
    let (guard, _reload) = init_with_reload(log_dir, level)?;
    Ok(guard)
}

/// 初始化 JSONL 文件、debug 控制台和可热更新的过滤层。
///
/// 返回值中的 guard 必须存活到进程退出，否则异步 writer 可能来不及落盘；
/// handle 可以跨线程克隆，用于配置监听器更新日志等级。
pub fn init_with_reload<L>(log_dir: &Path, level: L) -> Result<(WorkerGuard, ReloadHandle), DiagnosticsError>
where
    L: AsRef<str>,
{
    std::fs::create_dir_all(log_dir)?;
    let requested_level = level.as_ref().parse::<LogLevel>().unwrap_or(if cfg!(debug_assertions) {
        LogLevel::Debug
    } else {
        LogLevel::Info
    });
    let filter = EnvFilter::new(requested_level.as_str());
    let (filter_layer, filter_handle) = reload::Layer::new(filter);

    // `rolling::daily` 在打开文件失败时会 panic；builder API 保留 InitError，
    // 让只读目录、ACL 或磁盘故障成为可报告的启动错误。
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("codex-taskbar.jsonl")
        .build(log_dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer =
        tracing_subscriber::fmt::layer().json().with_current_span(false).with_span_list(false).with_writer(file_writer);

    let subscriber = tracing_subscriber::registry().with(filter_layer).with(file_layer);

    #[cfg(debug_assertions)]
    let subscriber = subscriber.with(tracing_subscriber::fmt::layer().compact().with_writer(std::io::stderr));

    subscriber.try_init()?;

    let current = Arc::new(RwLock::new(requested_level));
    let callback: Arc<ReloadCallback> =
        Arc::new(move |level| filter_handle.reload(EnvFilter::new(level.as_str())).map_err(|error| error.to_string()));
    Ok((guard, ReloadHandle { callback, current }))
}

/// `init_with_reload` 的命名别名。
pub fn init_reloading<L>(log_dir: &Path, level: L) -> Result<(WorkerGuard, ReloadHandle), DiagnosticsError>
where
    L: AsRef<str>,
{
    init_with_reload(log_dir, level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_updates_level_only_after_callback_succeeds() {
        let current = Arc::new(RwLock::new(LogLevel::Info));
        let handle = ReloadHandle { callback: Arc::new(|_| Ok(())), current: Arc::clone(&current) };
        handle.reload(LogLevel::Trace).unwrap();
        assert_eq!(handle.current_level(), LogLevel::Trace);

        let failing = ReloadHandle { callback: Arc::new(|_| Err("reload failed".into())), current };
        assert!(failing.reload(LogLevel::Debug).is_err());
        assert_eq!(failing.current_level(), LogLevel::Trace);
    }

    #[test]
    fn pruning_deletes_only_expired_codex_taskbar_log_files() {
        let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let directory = std::env::temp_dir().join(format!("codex-taskbar-log-prune-{unique}"));
        std::fs::create_dir_all(&directory).unwrap();
        let owned = directory.join("codex-taskbar.jsonl.2026-08-01");
        let unrelated = directory.join("another-app.jsonl.2026-08-01");
        std::fs::write(&owned, b"owned").unwrap();
        std::fs::write(&unrelated, b"unrelated").unwrap();

        let simulated_future = SystemTime::now() + Duration::from_secs(2 * 86_400);
        assert_eq!(prune_old_logs_at(&directory, 1, simulated_future).unwrap(), 1);
        assert!(!owned.exists());
        assert!(unrelated.exists());

        std::fs::remove_file(unrelated).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

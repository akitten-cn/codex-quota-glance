//! Codex CLI 的显式、安全定位器。
//!
//! 定位器不读取隐式环境变量，也不把用户目录写入缓存。调用方必须显式传入
//! `manual_path`、LocalAppData 根目录和 PATH 候选，这让 UI、服务和测试可以各自
//! 控制数据范围。所有候选都会拒绝 WindowsApps、目录和明显不是 Codex CLI 的
//! 文件名；探测失败时继续尝试下一候选。

use crate::supervisor::{StdioTransportConfig, configure_background_command};
use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// 显式传入的定位输入，不会在内部调用 `std::env::var` 或读取用户环境。
#[derive(Debug, Clone, Default)]
pub struct CodexCliLocatorInput {
    /// 手工指定的可执行文件，优先级最高。
    pub manual_path: Option<PathBuf>,
    /// `%LOCALAPPDATA%` 的值；定位器只扫描其下的 `OpenAI/Codex/bin`。
    pub local_app_data: Option<PathBuf>,
    /// 调用方预先解析好的 PATH 候选，按传入顺序检查。
    pub path_candidates: Vec<PathBuf>,
}

impl CodexCliLocatorInput {
    #[must_use]
    pub fn new(manual_path: Option<PathBuf>, local_app_data: Option<PathBuf>, path_candidates: Vec<PathBuf>) -> Self {
        Self { manual_path, local_app_data, path_candidates }
    }
}

/// 候选来源；只暴露来源类别，不把完整路径用于安全日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCliSource {
    Manual,
    LocalAppData { version_directory: String },
    Path,
}

/// 成功定位的 CLI。完整路径只用于真正启动进程；安全诊断请使用
/// [`LocatedCodexCli::safe_summary`]。
#[derive(Clone, PartialEq, Eq)]
pub struct LocatedCodexCli {
    path: PathBuf,
    source: CodexCliSource,
}

impl std::fmt::Debug for LocatedCodexCli {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug 输出也必须遵守脱敏约束，完整路径只能通过 path() 显式取得。
        formatter.debug_struct("LocatedCodexCli").field("safe_summary", &self.safe_summary()).finish_non_exhaustive()
    }
}

impl LocatedCodexCli {
    /// 返回启动所需的完整路径；调用方不应直接把它写日志。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn source(&self) -> &CodexCliSource {
        &self.source
    }

    /// 只包含文件名和来源类别，不包含盘符、用户目录或参数。
    #[must_use]
    pub fn safe_summary(&self) -> SafeCliSummary {
        SafeCliSummary {
            file_name: self.path.file_name().and_then(|name| name.to_str()).unwrap_or("codex").to_owned(),
            source: self.source.clone(),
        }
    }

    /// 生成 stdio transport 配置。参数固定包含 `app-server` 与 `--stdio`。
    #[must_use]
    pub fn transport_config(&self) -> StdioTransportConfig {
        StdioTransportConfig {
            program: self.path.as_os_str().to_owned(),
            args: vec![OsString::from("app-server"), OsString::from("--stdio")],
            ..StdioTransportConfig::default()
        }
    }
}

/// 可安全记录的定位摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeCliSummary {
    pub file_name: String,
    pub source: CodexCliSource,
}

/// 探测失败的非敏感分类；不保存 stdout/stderr 或命令行路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailure {
    ProtectedPackage,
    TimedOut,
    Failed,
}

/// `CapabilityProbe` 允许测试注入伪探测器，生产环境使用短时无副作用探测。
pub trait CapabilityProbe: Send + Sync {
    fn probe(&self, executable: &Path) -> Result<(), ProbeFailure>;
}

/// 真实 CLI 能力探测器。它只运行 `app-server --help`，stdout/stderr 丢弃，且
/// 到时会终止子进程；不会发送 prompt、读取认证信息或启动长期服务。
#[derive(Debug, Clone, Copy)]
pub struct ProcessCapabilityProbe {
    timeout: Duration,
}

impl Default for ProcessCapabilityProbe {
    fn default() -> Self {
        Self { timeout: Duration::from_millis(800) }
    }
}

impl ProcessCapabilityProbe {
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl CapabilityProbe for ProcessCapabilityProbe {
    fn probe(&self, executable: &Path) -> Result<(), ProbeFailure> {
        if is_windows_apps(executable) {
            return Err(ProbeFailure::ProtectedPackage);
        }
        let mut command = Command::new(executable);
        command.arg("app-server").arg("--help").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        configure_background_command(&mut command);
        let mut child = command.spawn().map_err(|_| ProbeFailure::Failed)?;
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err(ProbeFailure::Failed),
                Ok(None) if start.elapsed() < self.timeout => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeFailure::TimedOut);
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeFailure::Failed);
                }
            }
        }
    }
}

/// 定位失败；错误变体不携带完整候选路径，避免意外泄露用户目录。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LocatorError {
    #[error("未找到可用的 Codex CLI")]
    NotFound,
    #[error("候选位于受保护的 WindowsApps 包目录")]
    ProtectedPackage,
    #[error("手工指定的 Codex CLI 路径不是文件")]
    ManualNotFile,
    #[error("手工指定的文件名不是 codex/codex.exe")]
    ManualInvalidFilename,
    #[error("所有 Codex CLI 候选的能力探测均失败")]
    ProbeFailed,
}

/// 使用注入 probe 的 Codex CLI 定位器。
#[derive(Debug, Clone)]
pub struct CodexCliLocator<P = ProcessCapabilityProbe> {
    probe: P,
}

impl Default for CodexCliLocator<ProcessCapabilityProbe> {
    fn default() -> Self {
        Self { probe: ProcessCapabilityProbe::default() }
    }
}

impl<P: CapabilityProbe> CodexCliLocator<P> {
    #[must_use]
    pub fn with_probe(probe: P) -> Self {
        Self { probe }
    }

    /// 按 manual > LocalAppData 版本目录 > PATH 顺序定位并探测。
    pub fn locate(&self, input: &CodexCliLocatorInput) -> Result<LocatedCodexCli, LocatorError> {
        let mut protected_seen = false;
        let mut candidates = Vec::new();

        if let Some(manual) = &input.manual_path {
            if is_windows_apps(manual) {
                return Err(LocatorError::ProtectedPackage);
            }
            if !manual.is_file() {
                return Err(LocatorError::ManualNotFile);
            }
            if !is_codex_filename(manual) {
                return Err(LocatorError::ManualInvalidFilename);
            }
            candidates.push((manual.clone(), CodexCliSource::Manual));
        } else if let Some(local_app_data) = &input.local_app_data {
            for (path, version) in local_candidates(local_app_data) {
                if is_windows_apps(&path) {
                    protected_seen = true;
                } else {
                    candidates.push((path, CodexCliSource::LocalAppData { version_directory: version }));
                }
            }
        }

        // PATH 候选永远是最后一级；这也允许 LocalAppData 中损坏的旧版本回退到 PATH。
        for path in &input.path_candidates {
            if is_windows_apps(path) {
                protected_seen = true;
            } else if path.is_file() && is_codex_filename(path) {
                candidates.push((path.clone(), CodexCliSource::Path));
            }
        }

        if candidates.is_empty() {
            return Err(if protected_seen { LocatorError::ProtectedPackage } else { LocatorError::NotFound });
        }

        let mut probed = false;
        for (path, source) in candidates {
            match self.probe.probe(&path) {
                Ok(()) => return Ok(LocatedCodexCli { path, source }),
                Err(ProbeFailure::ProtectedPackage) => protected_seen = true,
                Err(ProbeFailure::TimedOut | ProbeFailure::Failed) => probed = true,
            }
        }
        if probed || protected_seen { Err(LocatorError::ProbeFailed) } else { Err(LocatorError::NotFound) }
    }
}

/// 不需要显式创建 locator 的便捷 API。
pub fn locate_codex_cli<P: CapabilityProbe>(
    input: &CodexCliLocatorInput,
    probe: P,
) -> Result<LocatedCodexCli, LocatorError> {
    CodexCliLocator::with_probe(probe).locate(input)
}

fn local_candidates(local_app_data: &Path) -> Vec<(PathBuf, String)> {
    let bin = local_app_data.join("OpenAI").join("Codex").join("bin");
    let Ok(entries) = fs::read_dir(bin) else { return Vec::new() };
    let mut candidates = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            if !metadata.is_dir() {
                return None;
            }
            let executable = path.join("codex.exe");
            if !executable.is_file() || !is_codex_filename(&executable) {
                return None;
            }
            let modified =
                executable.metadata().and_then(|metadata| metadata.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            let version = path.file_name()?.to_string_lossy().into_owned();
            Some((executable, version, modified))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| match right.2.cmp(&left.2) {
        Ordering::Equal => right.1.cmp(&left.1),
        ordering => ordering,
    });
    candidates.into_iter().map(|(path, version, _)| (path, version)).collect()
}

fn is_codex_filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("codex") || name.eq_ignore_ascii_case("codex.exe"))
        .unwrap_or(false)
}

fn is_windows_apps(path: &Path) -> bool {
    path.components().any(|component| component.as_os_str().to_string_lossy().eq_ignore_ascii_case("WindowsApps"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs::{self, File};
    use std::sync::Mutex;
    use std::time::SystemTime;

    #[derive(Clone, Default)]
    struct FakeProbe {
        accepted: Arc<Mutex<HashSet<String>>>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    use std::sync::Arc;

    impl CapabilityProbe for FakeProbe {
        fn probe(&self, executable: &Path) -> Result<(), ProbeFailure> {
            let name = executable.to_string_lossy().into_owned();
            self.seen.lock().unwrap().push(name.clone());
            if self.accepted.lock().unwrap().contains(&name) { Ok(()) } else { Err(ProbeFailure::Failed) }
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("codex-locator-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        File::create(path).unwrap();
    }

    #[test]
    fn priority_is_manual_then_local_then_path() {
        let root = temp_root("priority");
        let manual = root.join("manual").join("codex.exe");
        let local = root.join("local").join("OpenAI/Codex/bin/v2/codex.exe");
        let path = root.join("path").join("codex.exe");
        file(&manual);
        file(&local);
        file(&path);
        let probe = FakeProbe::default();
        probe.accepted.lock().unwrap().insert(manual.to_string_lossy().into_owned());
        let located = CodexCliLocator::with_probe(probe)
            .locate(&CodexCliLocatorInput::new(Some(manual.clone()), Some(root.join("local")), vec![path]))
            .unwrap();
        assert_eq!(located.source(), &CodexCliSource::Manual);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_version_upgrade_is_sorted_by_mtime_and_falls_back_after_probe_failure() {
        let root = temp_root("upgrade");
        let old = root.join("OpenAI").join("Codex").join("bin").join("v1").join("codex.exe");
        let new = root.join("OpenAI").join("Codex").join("bin").join("v2").join("codex.exe");
        file(&old);
        std::thread::sleep(Duration::from_millis(5));
        file(&new);
        let probe = FakeProbe::default();
        probe.accepted.lock().unwrap().insert(old.to_string_lossy().into_owned());
        let located = CodexCliLocator::with_probe(probe)
            .locate(&CodexCliLocatorInput::new(None, Some(root.clone()), Vec::new()))
            .unwrap();
        assert_eq!(located.path(), old);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_apps_and_wrong_files_are_rejected_without_fallback_leak() {
        let root = temp_root("protected");
        let windows = root.join("WindowsApps").join("codex.exe");
        file(&windows);
        let result = CodexCliLocator::with_probe(FakeProbe::default()).locate(&CodexCliLocatorInput::new(
            Some(windows),
            None,
            Vec::new(),
        ));
        assert_eq!(result, Err(LocatorError::ProtectedPackage));
        let wrong = root.join("codex-helper.exe");
        file(&wrong);
        let result = CodexCliLocator::with_probe(FakeProbe::default()).locate(&CodexCliLocatorInput::new(
            Some(wrong),
            None,
            Vec::new(),
        ));
        assert_eq!(result, Err(LocatorError::ManualInvalidFilename));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn probe_fallback_and_safe_summary_do_not_expose_path() {
        let root = temp_root("fallback");
        let first = root.join("first").join("codex.exe");
        let second = root.join("second").join("codex.exe");
        file(&first);
        file(&second);
        let probe = FakeProbe::default();
        probe.accepted.lock().unwrap().insert(second.to_string_lossy().into_owned());
        let located = CodexCliLocator::with_probe(probe)
            .locate(&CodexCliLocatorInput::new(None, None, vec![first, second.clone()]))
            .unwrap();
        assert_eq!(located.path(), second);
        let summary = located.safe_summary();
        assert_eq!(summary.file_name, "codex.exe");
        assert!(!format!("{summary:?}").contains(&root.to_string_lossy().to_string()));
        assert!(!format!("{located:?}").contains(&root.to_string_lossy().to_string()));
        let config = located.transport_config();
        assert!(config.args.iter().any(|arg| arg == "app-server"));
        assert!(config.args.iter().any(|arg| arg == "--stdio"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_root_is_not_found() {
        let root = std::env::temp_dir()
            .join(format!("missing-codex-locator-{}", SystemTime::now().elapsed().unwrap().as_nanos()));
        let result = CodexCliLocator::with_probe(FakeProbe::default()).locate(&CodexCliLocatorInput::new(
            None,
            Some(root),
            Vec::new(),
        ));
        assert_eq!(result, Err(LocatorError::NotFound));
    }
}

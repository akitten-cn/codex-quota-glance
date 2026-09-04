//! Mac GUI 专用 CLI 发现：仅输出类别诊断，不读取/复制认证文件。
use codex_taskbar_adapters_codex_app_server::{
    CapabilityProbe, CodexCliLocator, CodexCliLocatorInput, ProbeFailure, StdioTransportConfig,
};
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use std::io::Read;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct Discovery {
    pub config: Option<StdioTransportConfig>,
    pub report: Value,
}

pub fn candidates(home: &Path, path: &OsStr) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("CODEX_TASKBAR_BUNDLED_CLI") {
        paths.push(PathBuf::from(p));
    }
    for base in [PathBuf::from("/Applications"), home.join("Applications")] {
        for app in ["ChatGPT.app", "Codex.app"] {
            paths.push(base.join(app).join("Contents/Resources/codex"));
        }
    }
    paths.extend(std::env::split_paths(path).map(|p| p.join("codex")));
    paths.extend([
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        home.join(".local/bin/codex"),
        home.join(".cargo/bin/codex"),
        home.join(".volta/bin/codex"),
        home.join(".npm-global/bin/codex"),
    ]);
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    paths
}

// 在后台发现线程中执行。只取固定标记之间的 PATH，丢弃启动脚本的其他输出。
fn login_path() -> Option<OsString> {
    #[cfg(target_os = "macos")]
    {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        let mut child = Command::new(shell)
            .args(["-l", "-i", "-c", "printf '\\n__CT_PATH_BEGIN__%s__CT_PATH_END__\\n' \"$PATH\""])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.take(65536).read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let data = rx.recv_timeout(Duration::from_millis(500)).ok()?;
        let text = String::from_utf8(data).ok()?;
        let path = text.split("__CT_PATH_BEGIN__").nth(1)?.split("__CT_PATH_END__").next()?;
        if path.contains('\n') || path.len() > 16384 {
            return None;
        }
        return Some(OsString::from(path));
    }
    #[cfg(not(target_os = "macos"))]
    None
}

struct GuiProbe {
    path: OsString,
    results: std::sync::Mutex<Vec<Value>>,
    started: Instant,
}
impl CapabilityProbe for GuiProbe {
    fn probe(&self, executable: &Path) -> Result<(), ProbeFailure> {
        let result = (|| {
            if self.started.elapsed() > Duration::from_secs(24) {
                return Err(ProbeFailure::TimedOut);
            }
            let mut child = Command::new(executable)
                .args(["app-server", "--help"])
                .env("PATH", &self.path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| ProbeFailure::Failed)?;
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return if status.success() { Ok(()) } else { Err(ProbeFailure::Failed) },
                    Ok(None) if start.elapsed() < Duration::from_secs(8) => {
                        std::thread::sleep(Duration::from_millis(25))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ProbeFailure::TimedOut);
                    }
                }
            }
        })();
        self.results.lock().unwrap().push(json!({"result":match result {Ok(_)=>"ok",Err(ProbeFailure::TimedOut)=>"timeout",Err(_)=>"launch_or_probe_failed"}}));
        result
    }
}

pub fn discover(home: &Path, codex_home: &Path, manual: Option<PathBuf>) -> Discovery {
    let login = login_path();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = login.clone().unwrap_or(inherited);
    let paths = candidates(home, &path);
    let existing = paths.iter().filter(|p| p.is_file()).count();
    let probe = GuiProbe { path: path.clone(), results: Default::default(), started: Instant::now() };
    // 只保留汇总结果；路径、Shell 输出、OAuth、环境变量均不进入诊断。
    let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    struct RecordingProbe {
        inner: GuiProbe,
        records: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }
    impl CapabilityProbe for RecordingProbe {
        fn probe(&self, p: &Path) -> Result<(), ProbeFailure> {
            let r = self.inner.probe(p);
            *self.records.lock().unwrap() = self.inner.results.lock().unwrap().clone();
            r
        }
    }
    let located = CodexCliLocator::with_probe(RecordingProbe { inner: probe, records: results.clone() })
        .locate(&CodexCliLocatorInput::new(manual, None, paths));
    let code = if located.is_ok() {
        "connecting"
    } else if existing == 0 {
        "cli_not_found"
    } else {
        "cli_probe_failed"
    };
    let config = located.ok().map(|cli| {
        let mut c = cli.transport_config();
        c.args = vec!["app-server".into()];
        c.env = vec![("PATH".into(), path), ("CODEX_HOME".into(), codex_home.as_os_str().to_owned())];
        c.current_dir = Some(home.to_path_buf());
        c
    });
    Discovery {
        config,
        report: json!({"code":code,"existing_candidates":existing,"login_path_available":login.is_some(),"probes":*results.lock().unwrap()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn includes_both_desktop_bundles_and_custom_path() {
        let home = Path::new("/Users/example");
        let path = std::env::join_paths([home.join("node/bin")]).unwrap();
        let paths = candidates(home, &path);
        assert!(paths.contains(&PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex")));
        assert!(paths.contains(&home.join("Applications/Codex.app/Contents/Resources/codex")));
        assert!(paths.contains(&home.join("node/bin/codex")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_probe_accepts_readonly_help_stub_without_auth() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("codex-gui-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("codex");
        std::fs::write(&exe, b"#!/bin/sh\n[ \"$1\" = app-server ] && [ \"$2\" = --help ]\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let probe = GuiProbe { path: "/usr/bin:/bin".into(), results: Default::default(), started: Instant::now() };
        assert_eq!(probe.probe(&exe), Ok(()));
        let records = serde_json::to_string(&*probe.results.lock().unwrap()).unwrap();
        assert!(!records.contains(root.to_str().unwrap()));
        std::fs::remove_file(exe).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

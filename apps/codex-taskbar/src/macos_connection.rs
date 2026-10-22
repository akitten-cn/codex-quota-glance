//! Mac GUI 专用 CLI 发现：仅输出类别诊断，不读取/复制认证文件。
use codex_taskbar_adapters_codex_app_server::{
    CapabilityProbe, CodexCliLocator, CodexCliLocatorInput, LocatorError, ProbeFailure, StdioTransportConfig,
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

pub fn desktop_bundles(value: &Value) -> Option<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = serde_json::from_value(value.clone()).ok()?;
    if paths.len() > 32 || paths.iter().any(|p| !p.is_absolute() || p.extension() != Some(OsStr::new("app"))) {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    Some(paths.into_iter().filter(|p| seen.insert(p.clone())).collect())
}

// 不读取包内文件内容，也不追随目录链接。目录/条目预算限制损坏或异常大的包的扫描开销。
fn bundle_candidates(bundle: &Path) -> Vec<PathBuf> {
    let resources = bundle.join("Contents/Resources");
    let mut paths = vec![
        resources.join("codex-cli/CodexCLI.app/Contents/MacOS/codex"),
        resources.join("codex-cli/bin/codex"),
        resources.join("codex"),
        resources.join("bin/codex"),
    ];
    // 常见 npm 原生 CLI 布局直接列举，避免大型 node_modules 耗尽后备扫描预算。
    for package in ["node_modules/@openai/codex", "app.asar.unpacked/node_modules/@openai/codex"] {
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
            paths.push(resources.join(package).join("vendor").join(target).join("codex/codex"));
        }
    }
    let mut pending = vec![(resources, 0)];
    let mut remaining = 4096;
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            if remaining == 0 {
                return paths;
            }
            remaining -= 1;
            let path = entry.path();
            if entry.file_name() == OsStr::new("codex") && path.is_file() {
                paths.push(path);
            } else if depth < 10 && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push((path, depth + 1));
            }
        }
    }
    paths
}

pub fn candidates(home: &Path, path: &OsStr, desktop_bundles: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("CODEX_TASKBAR_BUNDLED_CLI") {
        paths.push(PathBuf::from(p));
    }
    let mut bundles = desktop_bundles.to_vec();
    for base in [PathBuf::from("/Applications"), home.join("Applications")] {
        for app in ["ChatGPT.app", "Codex.app"] {
            bundles.push(base.join(app));
        }
    }
    let mut seen_bundles = std::collections::HashSet::new();
    for bundle in bundles.into_iter().filter(|p| seen_bundles.insert(p.clone())) {
        paths.extend(bundle_candidates(&bundle));
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

fn connection_code(result: &Result<impl Sized, LocatorError>) -> &'static str {
    match result {
        Ok(_) => "connecting",
        Err(LocatorError::NotFound) => "cli_not_found",
        Err(LocatorError::ManualNotFile | LocatorError::ManualInvalidFilename | LocatorError::ProtectedPackage) => {
            "cli_path_invalid"
        }
        Err(LocatorError::ProbeFailed) => "cli_probe_failed",
    }
}

pub fn discover(home: &Path, codex_home: &Path, manual: Option<PathBuf>, desktop_bundles: &[PathBuf]) -> Discovery {
    let login = login_path();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = login.clone().unwrap_or(inherited);
    let paths = candidates(home, &path, desktop_bundles);
    let candidate_count = paths.len() + usize::from(manual.as_ref().is_some_and(|p| !paths.contains(p)));
    let mut existing_paths = std::collections::HashSet::new();
    for p in paths.iter().chain(manual.iter()).filter(|p| p.is_file()) {
        existing_paths.insert(p);
    }
    let existing = existing_paths.len();
    let manual_configured = manual.is_some();
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
    let code = connection_code(&located);
    let config = located.ok().map(|cli| {
        let mut c = cli.transport_config();
        c.args = vec!["app-server".into()];
        c.env = vec![("PATH".into(), path), ("CODEX_HOME".into(), codex_home.as_os_str().to_owned())];
        c.current_dir = Some(home.to_path_buf());
        c
    });
    Discovery {
        config,
        report: json!({"code":code,"candidate_count":candidate_count,"existing_candidates":existing,
            "desktop_bundles":desktop_bundles.len(),"manual_path_configured":manual_configured,
            "login_path_available":login.is_some(),"probes":*results.lock().unwrap()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BundleFixture(PathBuf);
    impl BundleFixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "codex-mac-bundles-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"test CLI; never executed").unwrap();
            path
        }
    }
    impl Drop for BundleFixture {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("codex-mac-bundles-"));
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn includes_both_desktop_bundles_and_custom_path() {
        let home = Path::new("/Users/example");
        let path = std::env::join_paths([home.join("node/bin")]).unwrap();
        let paths = candidates(home, &path, &[]);
        assert!(paths.contains(&PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex")));
        assert!(paths.contains(&home.join("Applications/Codex.app/Contents/Resources/codex")));
        assert!(paths.contains(&home.join("node/bin/codex")));
    }

    #[test]
    fn discovers_nested_cli_in_all_desktop_bundles_at_real_locations() {
        let fixture = BundleFixture::new();
        let incompatible = fixture.file("Downloads/ChatGPT.app/Contents/Resources/codex");
        let expected = fixture.file(
            "自定义 目录/Codex Preview.app/Contents/Resources/app.asar.unpacked/node_modules/@openai/codex/vendor/aarch64-apple-darwin/codex/codex",
        );
        let bundles = vec![fixture.0.join("Downloads/ChatGPT.app"), fixture.0.join("自定义 目录/Codex Preview.app")];
        let paths = candidates(&fixture.0, OsStr::new(""), &bundles);
        assert!(paths.contains(&incompatible));
        assert!(paths.contains(&expected));
        assert_eq!(paths.iter().filter(|p| **p == expected).count(), 1);
        struct AcceptOnly(PathBuf);
        impl CapabilityProbe for AcceptOnly {
            fn probe(&self, p: &Path) -> Result<(), ProbeFailure> {
                if p == self.0 { Ok(()) } else { Err(ProbeFailure::Failed) }
            }
        }
        let located = CodexCliLocator::with_probe(AcceptOnly(expected.clone()))
            .locate(&CodexCliLocatorInput::new(None, None, paths))
            .unwrap();
        assert_eq!(located.path(), expected);
    }

    #[test]
    fn chatgpt_desktop_codexcli_bundle_is_preferred_over_legacy_layout() {
        let fixture = BundleFixture::new();
        let expected = fixture.file("ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex");
        let wrapper = fixture.file("ChatGPT.app/Contents/Resources/codex-cli/bin/codex");
        let legacy = fixture.file("ChatGPT.app/Contents/Resources/codex");
        let paths = bundle_candidates(&fixture.0.join("ChatGPT.app"));
        assert_eq!(paths[0], expected);
        assert_eq!(paths[1], wrapper);
        assert!(paths.iter().position(|p| p == &legacy).unwrap() > 1);
    }

    #[test]
    fn refresh_bundle_payload_validates_and_deduplicates_real_paths() {
        let fixture = BundleFixture::new();
        let first = fixture.0.join("Codex Preview.app");
        let second = fixture.0.join("ChatGPT.app");
        assert_eq!(desktop_bundles(&json!([first, second, first])).unwrap(), vec![first, second]);
        for invalid in
            [json!(null), json!("/Applications/Codex.app"), json!(["relative/Codex.app"]), json!([fixture.0])]
        {
            assert!(desktop_bundles(&invalid).is_none());
        }
        assert_eq!(desktop_bundles(&json!([])), Some(vec![]));
    }

    #[test]
    fn discovery_distinguishes_no_cli_bad_manual_path_and_failed_probe() {
        assert_eq!(connection_code(&Ok::<(), _>(())), "connecting");
        assert_eq!(connection_code(&Err::<(), _>(LocatorError::NotFound)), "cli_not_found");
        assert_eq!(connection_code(&Err::<(), _>(LocatorError::ProbeFailed)), "cli_probe_failed");
        assert_eq!(connection_code(&Err::<(), _>(LocatorError::ManualNotFile)), "cli_path_invalid");
        assert_eq!(connection_code(&Err::<(), _>(LocatorError::ManualInvalidFilename)), "cli_path_invalid");
    }

    #[cfg(unix)]
    #[test]
    fn bundle_scan_does_not_follow_directory_symlinks_outside_resources() {
        let fixture = BundleFixture::new();
        fixture.file("private/codex");
        let bundle = fixture.0.join("Codex.app");
        let resources = bundle.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::os::unix::fs::symlink(fixture.0.join("private"), resources.join("external")).unwrap();
        assert!(!bundle_candidates(&bundle).contains(&resources.join("external/codex")));
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

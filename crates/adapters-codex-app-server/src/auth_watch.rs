//! 仅比较登录文件元数据；不打开、读取、复制或记录认证内容。
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    len: u64,
}

pub(crate) struct AuthWatch {
    path: PathBuf,
    stamp: Option<FileStamp>,
    next_check: Instant,
    missing_reload: Instant,
}

impl AuthWatch {
    pub fn new(path: PathBuf) -> Self {
        let stamp = Self::read_stamp(&path).ok().flatten();
        Self { path, stamp, next_check: Instant::now(), missing_reload: Instant::now() + Duration::from_secs(60) }
    }

    fn read_stamp(path: &std::path::Path) -> std::io::Result<Option<FileStamp>> {
        match fs::metadata(path) {
            Ok(m) => Ok(Some(FileStamp { modified: m.modified().ok(), created: m.created().ok(), len: m.len() })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn changed(&mut self, now: Instant) -> bool {
        if now < self.next_check {
            return false;
        }
        self.next_check = now + Duration::from_secs(2);
        let Ok(current) = Self::read_stamp(&self.path) else {
            return false;
        };
        let changed = current != self.stamp;
        self.stamp = current;
        // Keychain 等后端没有 auth.json 通知可用，定期重建只读连接以重新加载登录。
        let missing_reload = self.stamp.is_none() && now >= self.missing_reload;
        if changed || missing_reload {
            self.missing_reload = now + Duration::from_secs(60);
        }
        changed || missing_reload
    }
}

pub(crate) fn auth_path(config: &crate::StdioTransportConfig) -> Option<PathBuf> {
    config
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|h| PathBuf::from(h).join(".codex"))
        })
        .map(|home| home.join("auth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_external_login_replacement_logout_and_same_size_updates() {
        let root = std::env::temp_dir().join(format!(
            "taskbar-auth-watch-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("auth.json");
        fs::write(&path, b"synthetic-account-a").unwrap();
        let mut watch = AuthWatch::new(path.clone());
        let now = Instant::now();
        assert!(!watch.changed(now));
        fs::write(&path, b"synthetic-account-b").unwrap();
        let modified = SystemTime::now() + Duration::from_secs(1);
        fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        assert!(watch.changed(now + Duration::from_secs(3)));
        assert!(!watch.changed(now + Duration::from_secs(6)));
        fs::remove_file(&path).unwrap();
        assert!(watch.changed(now + Duration::from_secs(9)));
        assert!(!watch.changed(now + Duration::from_secs(12)));
        fs::write(&path, b"synthetic-account-c").unwrap();
        assert!(watch.changed(now + Duration::from_secs(15)));
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}

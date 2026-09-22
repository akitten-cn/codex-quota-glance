//! 按应用数据目录持有进程锁；连接关闭或进程崩溃时由操作系统释放。
use rusqlite::{Connection, ErrorCode};
use std::path::Path;

pub struct InstanceGuard {
    _connection: Connection,
}

impl InstanceGuard {
    pub fn acquire(root: &Path) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(root)?;
        let connection = Connection::open(root.join("instance-lock.sqlite"))?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        match connection.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => Ok(Some(Self { _connection: connection })),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(error.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_excludes_second_writer_and_releases_on_drop() {
        let root = std::env::temp_dir().join(format!(
            "taskbar-instance-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let first = InstanceGuard::acquire(&root).unwrap().unwrap();
        assert!(InstanceGuard::acquire(&root).unwrap().is_none());
        drop(first);
        assert!(InstanceGuard::acquire(&root).unwrap().is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn subprocess_lock_holder() {
        let Some(root) = std::env::var_os("TASKBAR_TEST_LOCK_ROOT") else { return };
        let root = std::path::PathBuf::from(root);
        let _guard = InstanceGuard::acquire(&root).unwrap().unwrap();
        std::fs::write(root.join("ready"), b"").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn separate_process_excludes_writer_and_forced_exit_releases_lock() {
        let root = std::env::temp_dir()
            .join(format!("taskbar-process-lock-{}", time::OffsetDateTime::now_utc().unix_timestamp_nanos()));
        std::fs::create_dir_all(&root).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "single_instance::tests::subprocess_lock_holder", "--nocapture"])
            .env("TASKBAR_TEST_LOCK_ROOT", &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ready = root.join("ready").exists();
        let excluded = ready && InstanceGuard::acquire(&root).unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready && excluded);
        assert!(InstanceGuard::acquire(&root).unwrap().is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}

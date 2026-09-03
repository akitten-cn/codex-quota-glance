//! Codex App Server 子进程启动配置和重启退避策略。
//!
//! 本模块不读取用户目录，也不会在配置构造或退避计算时启动进程。只有显式调用
//! [`AppServerSupervisor::spawn`] 才会执行配置中的命令；单元测试可以只测试
//! [`ExponentialBackoff`] 或向 transport 注入内存流。

use crate::transport::{
    AppServerTransport, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_MAX_LINE_BYTES, DEFAULT_REQUEST_TIMEOUT, LineCodec,
    TransportError, TransportEvent,
};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use thiserror::Error;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows GUI 常驻程序启动控制台子进程时必须抑制控制台窗口，否则 `codex
/// app-server` 会覆盖设置页或详情卡，看起来像菜单没有响应。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `codex app-server` 的启动配置。默认值只描述命令，不会自动执行。
#[derive(Debug, Clone)]
pub struct StdioTransportConfig {
    /// 可执行文件，默认是 PATH 中的 `codex`。
    pub program: OsString,
    /// 默认参数为 `app-server`。
    pub args: Vec<OsString>,
    /// 可选的工作目录；未设置时继承当前进程工作目录。
    pub current_dir: Option<PathBuf>,
    /// 额外环境变量；默认不读取或复制当前环境之外的数据。
    pub env: Vec<(OsString, OsString)>,
    pub client_name: String,
    pub client_version: String,
    pub max_line_bytes: usize,
    pub request_timeout: Duration,
    pub handshake_timeout: Duration,
}

impl Default for StdioTransportConfig {
    fn default() -> Self {
        Self {
            program: OsString::from("codex"),
            args: vec![OsString::from("app-server")],
            current_dir: None,
            env: Vec::new(),
            client_name: String::from("codex-taskbar"),
            client_version: String::from(env!("CARGO_PKG_VERSION")),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

impl StdioTransportConfig {
    /// 构造命令但不执行，便于在启动前检查参数。
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        configure_background_command(&mut command);
        command
    }

    /// 将配置映射成 transport 启动参数。
    #[must_use]
    pub fn codec(&self) -> LineCodec {
        LineCodec::new(self.max_line_bytes)
    }
}

/// 把外部 Codex 进程配置为后台进程，不创建可见控制台。
pub(crate) fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// 子进程启动错误。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    #[error("启动 Codex App Server 失败：{0}")]
    Spawn(String),
    #[error("Codex App Server 缺少 stdin/stdout 管道")]
    MissingPipe,
    #[error("transport 错误：{0}")]
    Transport(TransportError),
}

impl From<TransportError> for SupervisorError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

/// 饱和指数退避。延迟计算不做浮点运算，避免不同平台的舍入差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExponentialBackoff {
    initial: Duration,
    maximum: Duration,
    multiplier: u32,
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self { initial: Duration::from_millis(250), maximum: Duration::from_secs(30), multiplier: 2 }
    }
}

impl ExponentialBackoff {
    /// 创建退避策略。`maximum` 会被用于饱和上限，乘数至少按 1 处理。
    #[must_use]
    pub const fn new(initial: Duration, maximum: Duration, multiplier: u32) -> Self {
        Self { initial, maximum, multiplier }
    }

    /// 计算第 `attempt` 次失败后的等待时长；attempt 从 0 开始。
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let mut delay = self.initial;
        let multiplier = self.multiplier.max(1);
        for _ in 0..attempt {
            delay = saturating_mul(delay, multiplier).min(self.maximum);
            if delay >= self.maximum {
                break;
            }
        }
        delay.min(self.maximum)
    }

    #[must_use]
    pub const fn initial(self) -> Duration {
        self.initial
    }

    #[must_use]
    pub const fn maximum(self) -> Duration {
        self.maximum
    }
}

/// 启动一次 Codex App Server stdio transport 的 supervisor 入口。
///
/// 当前实例只负责一次子进程生命周期；当进程退出时，事件 channel 会收到 EOF。
/// [`ExponentialBackoff`] 提供给上层重启策略，避免 supervisor 在 UI 线程中自行
/// 睡眠或无限重启。
#[derive(Debug, Clone, Copy, Default)]
pub struct AppServerSupervisor;

impl AppServerSupervisor {
    /// 显式启动配置中的外部命令。该方法会创建子进程和后台 transport 线程。
    pub fn spawn(
        &self,
        config: &StdioTransportConfig,
    ) -> Result<(AppServerTransport, mpsc::Receiver<TransportEvent>), SupervisorError> {
        let mut command = config.command();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| SupervisorError::Spawn(error.to_string()))?;
        let stdin = child.stdin.take().ok_or(SupervisorError::MissingPipe)?;
        let stdout = child.stdout.take().ok_or(SupervisorError::MissingPipe)?;
        // 单独等待线程负责回收句柄，避免 transport 返回后留下僵尸子进程；stdout
        // EOF 仍由 transport reader 线程负责向事件 channel 报告。
        std::thread::Builder::new()
            .name("codex-app-server-child-wait".to_owned())
            .spawn(move || {
                let _ = child.wait();
            })
            .map_err(|error| SupervisorError::Spawn(error.to_string()))?;
        Ok(AppServerTransport::spawn_with_io(
            stdout,
            stdin,
            config.client_name.clone(),
            config.client_version.clone(),
            config.codec(),
            config.request_timeout,
            config.handshake_timeout,
        ))
    }
}

fn saturating_mul(duration: Duration, multiplier: u32) -> Duration {
    let seconds = duration.as_secs().saturating_mul(u64::from(multiplier));
    let nanos = u64::from(duration.subsec_nanos()).saturating_mul(u64::from(multiplier));
    let carry = nanos / 1_000_000_000;
    let remainder = (nanos % 1_000_000_000) as u32;
    Duration::new(seconds.saturating_add(carry), remainder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_is_configuration_only() {
        let config = StdioTransportConfig::default();
        assert_eq!(config.program, OsString::from("codex"));
        assert_eq!(config.args, vec![OsString::from("app-server")]);
    }

    #[test]
    fn backoff_grows_and_saturates() {
        let policy = ExponentialBackoff::new(Duration::from_millis(100), Duration::from_millis(350), 2);
        assert_eq!(policy.delay_for(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for(2), Duration::from_millis(350));
        assert_eq!(policy.delay_for(20), Duration::from_millis(350));
    }

    #[test]
    fn multiplier_zero_is_safe() {
        let policy = ExponentialBackoff::new(Duration::from_millis(10), Duration::from_millis(100), 0);
        assert_eq!(policy.delay_for(4), Duration::from_millis(10));
    }
}

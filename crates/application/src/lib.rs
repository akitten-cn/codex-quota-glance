//! 应用用例与外部数据源端口。

use codex_taskbar_domain::usage::{TokenCounts, UsageSource};
use codex_taskbar_domain::{activity::ActivityState, quota::QuotaValue};
use thiserror::Error;

pub mod coordinator;
pub mod local_usage_ledger;
pub mod monitor;
pub mod scheduler;
pub mod ui_snapshot;

/// App Server 返回的一次原子额度快照。
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitSnapshot {
    pub five_hour: Option<QuotaValue>,
    pub weekly: Option<QuotaValue>,
    pub observed_at_unix_ms: i64,
    pub revision: u64,
}

/// 外部数据源返回的一次 Token 用量快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsageSnapshot {
    pub current_thread: Option<TokenCounts>,
    /// 最近一次 Turn 的增量；与 `current_thread` 的累计值分开展示。
    pub last_turn: Option<TokenCounts>,
    /// 模型上下文窗口上限；没有可靠当前占用值时不得据此计算百分比。
    pub model_context_window: Option<u64>,
    pub today: Option<TokenCounts>,
    pub observed_at_unix_ms: i64,
    pub source: UsageSource,
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("Codex App Server 当前不可用")]
    Unavailable,
    #[error("Codex App Server 协议不兼容：{0}")]
    Incompatible(String),
    #[error("Codex 数据解析失败：{0}")]
    InvalidData(String),
}

/// Codex 数据适配器必须实现的窄接口。
///
/// 首选实现是 App Server；SQLite/JSONL 适配器只能提供显式标记的降级数据。
pub trait CodexTelemetrySource {
    fn read_rate_limits(&mut self) -> Result<RateLimitSnapshot, TelemetryError>;
    fn current_activity(&mut self) -> Result<Vec<ActivityState>, TelemetryError>;

    /// 旧适配器可以暂时不支持 Token 统计；调用方必须把不兼容显示为 Unknown。
    fn read_token_usage(&mut self) -> Result<TokenUsageSnapshot, TelemetryError> {
        Err(TelemetryError::Incompatible("数据源不提供 Token 用量".to_owned()))
    }
}

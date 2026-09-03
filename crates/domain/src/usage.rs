//! Token 用量的稳定领域语义。
//!
//! App Server、SQLite 与未来其他来源的字段并不完全一致。本模块只保存 UI 确实需要的
//! 聚合值，并显式记录来源和新鲜度，避免把历史数据库中的旧数据显示成实时统计。

use serde::{Deserialize, Serialize};

/// Token 用量的数据来源。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    AppServer,
    /// 本机 Codex session JSONL 中新增的 `token_count` 事件。仅解析 Token
    /// 数字，不读取或持久化 Prompt、线程标题及其他会话正文。
    SessionLogFallback,
    SqliteFallback,
    #[default]
    None,
}

impl UsageSource {
    /// 该快照是否包含可以立即用于“本次消耗”展示的实时 Token 明细。
    #[must_use]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::AppServer | Self::SessionLogFallback)
    }
}

/// 一组允许部分字段缺失的 Token 计数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    /// 写入 Prompt Cache 的输入 Token。它属于输入侧明细，不参与总量二次相加。
    pub cache_write_input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning_output: Option<u64>,
    pub total: Option<u64>,
}

impl TokenCounts {
    /// 输入与输出齐全时始终按官方口径重新计算总量。不能优先信任旧版本或后备
    /// 数据源里的 `total`，因为历史版本可能曾把 cached input 重复加入该字段。
    /// 仅在明细不完整时才回退服务端 total；cached input 始终只是 input 的子集。
    #[must_use]
    pub fn display_total(&self) -> Option<u64> {
        match (self.input, self.output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => self.total,
        }
    }
}

/// UI 消费的 Token 状态；`last_known` 只用于诊断，不能冒充当前值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsageState {
    pub current_thread: Option<TokenCounts>,
    /// App Server 最近一次 Turn 的增量明细；不能与线程累计值相加。
    pub last_turn: Option<TokenCounts>,
    /// `last_turn` 有独立来源：App Server 的可用性变化不能清除本机 session
    /// 已捕获到的逐轮明细。
    #[serde(default)]
    pub last_turn_source: UsageSource,
    /// 当前模型上下文窗口上限。协议只提供窗口大小时，不推算虚假的占用率。
    pub model_context_window: Option<u64>,
    pub today: Option<TokenCounts>,
    /// `today` 的独立来源。当前线程可来自 App Server，而今天的补充累计量可来自
    /// 本机结构化 SQLite 增量账本，二者不能共用一个 freshness 标志。
    pub today_source: UsageSource,
    /// `true` 表示今日值是程序运行期间本机捕获的增量，并非官方完整日账单。
    pub today_is_partial: bool,
    pub source: UsageSource,
    pub fresh: bool,
    pub observed_at_unix_ms: i64,
    pub last_known: Option<TokenCounts>,
}

impl Default for TokenUsageState {
    fn default() -> Self {
        Self {
            current_thread: None,
            last_turn: None,
            last_turn_source: UsageSource::None,
            model_context_window: None,
            today: None,
            today_source: UsageSource::None,
            today_is_partial: false,
            source: UsageSource::None,
            fresh: false,
            observed_at_unix_ms: 0,
            last_known: None,
        }
    }
}

impl TokenUsageState {
    /// 数据源断开后清除任务栏可见的实时值，但保留最后一次观测用于诊断。
    pub fn mark_unavailable(&mut self) {
        self.last_known = self.current_thread.clone().or_else(|| self.last_known.clone());
        self.current_thread = None;
        if self.last_turn_source != UsageSource::SessionLogFallback {
            self.last_turn = None;
            self.last_turn_source = UsageSource::None;
        }
        self.model_context_window = None;
        // 本机增量账本不依赖 App Server 连通性；断开时仍可如实展示已捕获的
        // 今日总数，但必须保留“本机捕获”语义。
        let keep_local_today =
            matches!(self.today_source, UsageSource::SqliteFallback | UsageSource::SessionLogFallback);
        if !keep_local_today {
            self.today = None;
            self.today_source = UsageSource::None;
            self.today_is_partial = false;
        }
        self.source = UsageSource::None;
        self.fresh = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_is_not_double_counted() {
        let counts = TokenCounts {
            input: Some(100),
            cached_input: Some(80),
            cache_write_input: Some(10),
            output: Some(25),
            reasoning_output: None,
            total: None,
        };
        assert_eq!(counts.display_total(), Some(125));
    }

    #[test]
    fn detailed_counts_override_a_legacy_double_counted_total() {
        let counts = TokenCounts {
            input: Some(100),
            cached_input: Some(80),
            output: Some(25),
            total: Some(205),
            ..TokenCounts::default()
        };
        assert_eq!(counts.display_total(), Some(125));
    }

    #[test]
    fn unavailable_keeps_only_diagnostic_last_known() {
        let counts = TokenCounts { total: Some(42), ..TokenCounts::default() };
        let mut state = TokenUsageState {
            current_thread: Some(counts.clone()),
            last_turn: Some(counts.clone()),
            last_turn_source: UsageSource::AppServer,
            model_context_window: Some(200_000),
            today: Some(counts.clone()),
            today_source: UsageSource::AppServer,
            today_is_partial: false,
            source: UsageSource::AppServer,
            fresh: true,
            observed_at_unix_ms: 100,
            last_known: None,
        };
        state.mark_unavailable();
        assert_eq!(state.last_known, Some(counts));
        assert!(state.current_thread.is_none());
        assert!(state.last_turn.is_none());
        assert_eq!(state.model_context_window, None);
        assert!(!state.fresh);
    }
}

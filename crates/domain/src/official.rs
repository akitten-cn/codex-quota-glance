//! Codex 官方账户详情的协议无关领域模型。
//!
//! 官方订阅额度、账户级 Token 活动和本机线程 Token 是三组不同语义的数据。
//! 本模块只保存已经归一化、可以安全交给 UI 的字段，不包含邮箱原文、认证令牌、
//! App Server 原始 JSON 或任何可写账户操作。

/// 官方账户类型。`ApiKey` 与 ChatGPT 订阅额度不是同一种模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OfficialAccountKind {
    ChatGpt,
    ApiKey,
    AmazonBedrock,
    #[default]
    Unknown,
}

/// UI 对官方数据可用性的明确标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OfficialFreshness {
    Live,
    Cached,
    #[default]
    Unavailable,
}

/// 单个官方只读端点的数据状态。
///
/// 账户、额度和账户级 Token 来自三个相互独立的 RPC。它们必须分别记录最后
/// 成功时间，避免某一个端点成功后把另外两个端点的旧值一起标成实时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OfficialEndpointStatus {
    pub freshness: OfficialFreshness,
    pub observed_at_unix_ms: Option<i64>,
}

impl OfficialEndpointStatus {
    #[must_use]
    pub const fn live(observed_at_unix_ms: i64) -> Self {
        Self { freshness: OfficialFreshness::Live, observed_at_unix_ms: Some(observed_at_unix_ms) }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self { freshness: OfficialFreshness::Unavailable, observed_at_unix_ms: None }
    }

    /// 请求失败或连接断开时只降级已有成功值；从未成功的端点保持不可用。
    #[must_use]
    pub const fn cached(self) -> Self {
        if self.observed_at_unix_ms.is_some() {
            Self { freshness: OfficialFreshness::Cached, observed_at_unix_ms: self.observed_at_unix_ms }
        } else {
            Self::unavailable()
        }
    }
}

/// 经过脱敏的官方账户概要。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialAccount {
    pub kind: OfficialAccountKind,
    /// 只允许保存形如 `ab***@example.com` 的掩码标识。
    pub masked_identifier: Option<String>,
    pub plan_type: Option<String>,
    pub auth_mode: Option<String>,
    pub requires_openai_auth: Option<bool>,
}

/// ChatGPT Credits 信息。余额保留后端字符串，不擅自添加货币符号。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialCredits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

/// 账户个人消费控制快照。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OfficialSpendControl {
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining_percent: Option<f32>,
    pub resets_at_unix: Option<i64>,
}

/// 可用额度重置券的只读摘要；消费属于显式写操作，不在本模型中暴露。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialResetCredits {
    pub available_count: u64,
    /// 每张重置卡的到期时间，按最早到期排序。只保留时间，不保存卡 ID 或其它
    /// 可关联账户的字段，详情页据此逐张展示而不是只报“可用数量”。
    pub expiry_times_unix: Vec<i64>,
    pub nearest_expiry_unix: Option<i64>,
}

/// 官方账户某个自然日的 Token 用量。
///
/// `date` 始终是经过适配器校验、归一化后的 `YYYY-MM-DD`，不携带时区推断；
/// `tokens` 是服务端该日期桶提供的非负整数。领域层不保存畸形或来源不明的桶。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialDailyUsage {
    pub date: String,
    pub tokens: u64,
}

/// 官方端点针对当前或最近请求线程返回的服务端估算用量。
///
/// 金额字段来自 `account/tokenUsage/read`，是 billing route 可用时的估算值；
/// 它们不代表最终账单，也绝不能由客户端根据模型价格自行反推或补齐。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialThreadUsage {
    pub thread_id: String,
    /// 服务端估算的 USD 微美元（1 USD = 1_000_000 micros）。缺失表示服务端
    /// 没有可用 billing route，不能展示为 0。
    pub estimated_usage_usd_micros: Option<u64>,
    /// 服务端估算的 Credits 微单位；仅用于与 USD 估算并列展示，不换算货币。
    pub estimated_usage_credits_micros: u64,
    /// 按模型分组的服务端明细；保留响应顺序，不能跨模型相加为账单金额。
    pub groups: Vec<OfficialThreadUsageGroup>,
}

/// 当前/最近线程内单个模型的服务端估算明细。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialThreadUsageGroup {
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub net_new_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub speed: Option<String>,
    /// 服务端给出的该模型组 Credits 估算微单位，不转换为 USD。
    pub estimated_usage_credits_micros: u64,
}

/// 官方账户级 Token 活动摘要，来自 `account/usage/read`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfficialAccountUsage {
    pub lifetime_tokens: Option<u64>,
    pub peak_daily_tokens: Option<u64>,
    pub longest_running_turn_seconds: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
    /// 按日期升序排列、按日期去重后的近期日桶。
    pub daily_usage: Vec<OfficialDailyUsage>,
    /// 服务端返回的最近一个日桶；日期由后端定义，UI 不把它擅自改称“本地今日”。
    pub latest_daily_date: Option<String>,
    pub latest_daily_tokens: Option<u64>,
    /// App Server 为当前或最近请求线程提供的服务端估算，不与账户累计 Token 相加。
    pub thread_usage: Option<OfficialThreadUsage>,
}

/// 一次可提交给 UI 的官方账户快照。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OfficialSnapshot {
    pub account: Option<OfficialAccount>,
    pub credits: Option<OfficialCredits>,
    pub individual_limit: Option<OfficialSpendControl>,
    pub spend_control_reached: Option<bool>,
    pub reset_credits: Option<OfficialResetCredits>,
    pub account_usage: Option<OfficialAccountUsage>,
    pub rate_limit_name: Option<String>,
    pub rate_limit_reached_type: Option<String>,
    pub account_status: OfficialEndpointStatus,
    pub quota_status: OfficialEndpointStatus,
    pub usage_status: OfficialEndpointStatus,
    pub freshness: OfficialFreshness,
    pub observed_at_unix_ms: i64,
}

impl OfficialSnapshot {
    /// 连接断开时保留最后成功的只读快照，但强制标记为缓存，避免 UI 冒充实时。
    #[must_use]
    pub fn cached(mut self) -> Self {
        self.account_status = self.account_status.cached();
        self.quota_status = self.quota_status.cached();
        self.usage_status = self.usage_status.cached();
        if self.account.is_some()
            || self.credits.is_some()
            || self.individual_limit.is_some()
            || self.reset_credits.is_some()
            || self.account_usage.is_some()
            || self.spend_control_reached.is_some()
            || self.rate_limit_name.is_some()
            || self.rate_limit_reached_type.is_some()
        {
            self.freshness = OfficialFreshness::Cached;
        } else {
            self.freshness = OfficialFreshness::Unavailable;
        }
        self
    }

    /// 以最保守语义计算整张卡片的汇总状态；各区域仍应展示独立状态。
    pub fn refresh_aggregate_freshness(&mut self) {
        let statuses = [self.account_status.freshness, self.quota_status.freshness, self.usage_status.freshness];
        self.freshness = if statuses.iter().all(|status| *status == OfficialFreshness::Live) {
            OfficialFreshness::Live
        } else if statuses.iter().any(|status| *status != OfficialFreshness::Unavailable) {
            OfficialFreshness::Cached
        } else {
            OfficialFreshness::Unavailable
        };
        self.observed_at_unix_ms = [
            self.account_status.observed_at_unix_ms,
            self.quota_status.observed_at_unix_ms,
            self.usage_status.observed_at_unix_ms,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_snapshot_is_never_left_live() {
        let snapshot = OfficialSnapshot {
            account: Some(OfficialAccount { kind: OfficialAccountKind::ChatGpt, ..OfficialAccount::default() }),
            freshness: OfficialFreshness::Live,
            ..OfficialSnapshot::default()
        }
        .cached();
        assert_eq!(snapshot.freshness, OfficialFreshness::Cached);
    }
}

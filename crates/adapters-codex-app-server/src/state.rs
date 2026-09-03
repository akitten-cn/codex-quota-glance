//! App Server 稀疏通知的状态合并。
//!
//! `account/*/updated` 不是完整快照：字段可能缺失，也可能以 JSON null 出现。
//! 本模块采用 patch 语义——缺失和 null 都表示“本次没有可用更新”，绝不把已经
//! 观测到的值清掉；未知字段仍保存在 map/raw 中供诊断和未来协议版本使用。

use crate::activity::{ActivityEvent, parse_activity_event_value};
use crate::quota::RateLimitWindow;
use codex_taskbar_application::RateLimitSnapshot;
use codex_taskbar_domain::{
    official::{
        OfficialAccount, OfficialAccountKind, OfficialAccountUsage, OfficialCredits, OfficialDailyUsage,
        OfficialEndpointStatus, OfficialFreshness, OfficialResetCredits, OfficialSnapshot, OfficialSpendControl,
        OfficialThreadUsage, OfficialThreadUsageGroup,
    },
    quota::QuotaValue,
    usage::TokenCounts,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    time::{SystemTime, UNIX_EPOCH},
};

/// 已结束线程在仍有其他活动线程时继续参与聚合的提醒窗口。
///
/// 这样失败会短暂压过后台执行并提示用户，但不会让很久以前的失败永久把状态灯
/// 锁成红色。没有其他活动线程时仍回退到 `last_activity`，保持原有的完成/失败结果。
const TERMINAL_ACTIVITY_NOTICE_MS: i64 = 10_000;

/// 稀疏账户概要。未知字段保持在 `fields` 中，不强行猜测账户协议版本。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub fields: Map<String, Value>,
    pub requires_openai_auth: Option<bool>,
}

impl Deref for AccountSnapshot {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl DerefMut for AccountSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fields
    }
}

impl From<Map<String, Value>> for AccountSnapshot {
    fn from(fields: Map<String, Value>) -> Self {
        Self { fields, requires_openai_auth: None }
    }
}

impl AccountSnapshot {
    /// 按字段读取账户概要，便于上层读取 `email`、`planType` 等版本相关字段。
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// 稀疏额度状态；完整快照转换仍由 [`crate::parse_rate_limit_snapshot`] 完成。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitsSnapshot {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub updated_at_unix_ms: Option<i64>,
    /// 未知窗口、服务端新增字段等原样保留，避免兼容层静默丢数据。
    pub raw: Map<String, Value>,
    /// 完整读取才提供的 reset credits 摘要；稀疏通知不会清空它。
    pub reset_credits_raw: Option<Value>,
}

impl RateLimitsSnapshot {
    /// 将当前已合并窗口转换为 application 层原子快照；未知时长不被误分类。
    #[must_use]
    pub fn snapshot(&self, observed_at_unix_ms: i64, revision: u64) -> RateLimitSnapshot {
        let mut five_hour = None;
        let mut weekly = None;
        for window in [self.primary.as_ref(), self.secondary.as_ref()].into_iter().flatten() {
            let value = QuotaValue::from_used_percent(
                window.used_percent,
                Some(window.window_duration_mins),
                window.resets_at_unix,
            );
            match window.window_duration_mins {
                300 if five_hour.is_none() => five_hour = Some(value),
                10_080 if weekly.is_none() => weekly = Some(value),
                _ => {}
            }
        }
        RateLimitSnapshot { five_hour, weekly, observed_at_unix_ms, revision }
    }
}

/// `thread/tokenUsage/updated` 的兼容保存结构。
///
/// 目前只解析常见的线程标识和 token 字段，其余字段都在 `raw` 中保留；这样新
/// 版本增加缓存命中、上下文窗口等字段时，旧 UI 仍可安全运行。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsageUpdate {
    pub thread_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// 最近一次 Turn 的增量明细；与上面的线程累计值严格分开。
    pub last: Option<TokenCounts>,
    /// 当前模型的上下文窗口上限，不等同于已经占用的上下文。
    pub model_context_window: Option<u64>,
    /// 最近一次收到线程 Token 通知的 Unix 毫秒时间戳；活动通知不会刷新它。
    pub observed_at_unix_ms: Option<i64>,
    pub raw: Value,
}

/// `account/usage/read` 的账户级统计。保留原始兼容结构，但不会保存 prompt 或认证数据。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageSnapshot {
    pub raw: Value,
}

/// 适配器维护的纯内存状态；不包含进程句柄、文件路径或认证数据。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppServerState {
    pub account: AccountSnapshot,
    pub rate_limits: RateLimitsSnapshot,
    pub token_usage: Option<TokenUsageUpdate>,
    pub account_usage: Option<AccountUsageSnapshot>,
    pub last_activity: Option<ActivityEvent>,
    /// 按 Codex thread 保存最后活动，避免并行任务被“最后一条通知”互相覆盖。
    #[serde(default)]
    pub thread_activities: HashMap<String, ThreadActivityRecord>,
}

/// 单个 Codex thread 的最近状态及观测时间。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadActivityRecord {
    pub event: ActivityEvent,
    pub observed_at_unix_ms: i64,
}

impl AppServerState {
    /// 应用一个 JSON-RPC 通知；未知通知被忽略，协议层不会因新增事件而中断。
    pub fn apply_notification(&mut self, message: &Value) {
        self.apply_notification_at(message, now_unix_ms());
    }

    /// 带显式时间的通知合并入口，便于测试交错线程和终态提醒窗口。
    pub fn apply_notification_at(&mut self, message: &Value, observed_at_unix_ms: i64) {
        let Some(method) = message.get("method").and_then(Value::as_str) else { return };
        match method {
            "account/updated" => merge_account(&mut self.account, message),
            "account/rateLimits/updated" | "account/rate_limits/updated" => {
                merge_rate_limits(&mut self.rate_limits, message)
            }
            "thread/tokenUsage/updated" | "thread/token_usage/updated" => {
                merge_token_usage(&mut self.token_usage, message)
            }
            _ if method.starts_with("thread/")
                || method.starts_with("turn/")
                || method.starts_with("item/")
                || method.starts_with("approval/")
                || method.starts_with("execApproval/")
                || method.starts_with("requestUserInput/") =>
            {
                if let Some(event) = parse_activity_event_value(message) {
                    self.last_activity = Some(event.clone());
                    if let Some(thread_id) = event.thread_id.clone() {
                        self.thread_activities.insert(thread_id, ThreadActivityRecord { event, observed_at_unix_ms });
                    }
                }
            }
            _ => {}
        }
    }

    /// `apply_notification` 的消息流别名，方便上层统一处理 JSON-RPC 通知。
    pub fn apply_message(&mut self, message: &Value) {
        self.apply_notification(message);
    }

    /// 返回所有已知线程的聚合活动。
    ///
    /// 无 thread id 的事件只在没有可用线程状态时作为后备，避免低置信通知覆盖
    /// WaitingForUser / Executing 等明确线程状态。
    #[must_use]
    pub fn aggregated_activity(&self, now_unix_ms: i64) -> Option<ActivityEvent> {
        let mut candidates = self.thread_activities.values().filter(|record| {
            !is_terminal_activity(record.event.state)
                || now_unix_ms.saturating_sub(record.observed_at_unix_ms) <= TERMINAL_ACTIVITY_NOTICE_MS
        });
        candidates
            .next()
            .into_iter()
            .chain(candidates)
            .max_by_key(|record| record.event.state.priority())
            .map(|record| record.event.clone())
            .or_else(|| self.last_activity.clone())
    }
}

fn is_terminal_activity(state: codex_taskbar_domain::activity::ActivityState) -> bool {
    matches!(
        state,
        codex_taskbar_domain::activity::ActivityState::Idle
            | codex_taskbar_domain::activity::ActivityState::Completed
            | codex_taskbar_domain::activity::ActivityState::Failed
    )
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

/// 合并账户 patch。对象字段递归合并；null 和缺失字段均不会清除旧值。
pub fn merge_account(existing: &mut AccountSnapshot, update: &Value) {
    if let Some(value) = find_value(update, &["requiresOpenaiAuth", "requires_openai_auth"]).and_then(Value::as_bool) {
        existing.requires_openai_auth = Some(value);
    }
    if let Some(object) = nested_object(update, "account") {
        merge_object(&mut existing.fields, object);
    }
}

/// 应用 `account/read` 的完整账户结果；登录身份切换时必须清除旧标识与方案。
pub fn replace_account(existing: &mut AccountSnapshot, response: &Value) {
    let requires_openai_auth =
        find_value(response, &["requiresOpenaiAuth", "requires_openai_auth"]).and_then(Value::as_bool);
    let fields = find_value(response, &["account"]).and_then(Value::as_object).cloned().unwrap_or_default();
    *existing = AccountSnapshot { fields, requires_openai_auth };
}

/// 合并额度 patch。primary/secondary 的 null 只代表“未更新”，不会删除旧窗口。
pub fn merge_rate_limits(existing: &mut RateLimitsSnapshot, update: &Value) {
    let Some(object) = nested_object(update, "rateLimits").or_else(|| nested_object(update, "rate_limits")) else {
        return;
    };

    merge_rate_limit_window(&mut existing.primary, object.get("primary"));
    merge_rate_limit_window(&mut existing.secondary, object.get("secondary"));
    if let Some(timestamp) = object.get("updatedAt").or_else(|| object.get("updated_at")).and_then(number_i64) {
        existing.updated_at_unix_ms = Some(timestamp);
    }
    merge_object(&mut existing.raw, object);
}

/// 应用 `account/rateLimits/read` 的完整响应。
///
/// 与 rolling notification 不同，完整读取中缺失或 null 的窗口表示当前确实不存在，
/// 因此必须清除旧值；否则曾经出现过的 5h 会在后续 weekly-only 快照中“复活”。
pub fn replace_rate_limits(existing: &mut RateLimitsSnapshot, response: &Value) -> bool {
    let Some(object) = preferred_rate_limit_object(response) else {
        return false;
    };

    let mut primary = None;
    let mut secondary = None;
    merge_rate_limit_window(&mut primary, object.get("primary"));
    merge_rate_limit_window(&mut secondary, object.get("secondary"));
    existing.primary = primary;
    existing.secondary = secondary;
    existing.updated_at_unix_ms = object.get("updatedAt").or_else(|| object.get("updated_at")).and_then(number_i64);
    existing.raw = object.clone();
    existing.reset_credits_raw = find_value(response, &["rateLimitResetCredits", "rate_limit_reset_credits"]).cloned();
    true
}

/// 原子替换账户级 Token 活动；方法不支持时调用方不应伪造空响应。
pub fn replace_account_usage(existing: &mut Option<AccountUsageSnapshot>, response: &Value) -> bool {
    let Some(object) = response.as_object() else { return false };
    if !object.contains_key("summary")
        && !object.contains_key("dailyUsageBuckets")
        && !object.contains_key("daily_usage_buckets")
    {
        return false;
    }
    *existing = Some(AccountUsageSnapshot { raw: response.clone() });
    true
}

impl AppServerState {
    /// 将协议兼容状态投影为不含邮箱原文和原始 JSON 的官方领域快照。
    #[must_use]
    pub fn official_snapshot(&self, freshness: OfficialFreshness, observed_at_unix_ms: i64) -> OfficialSnapshot {
        let endpoint = |present: bool| {
            if present {
                OfficialEndpointStatus { freshness, observed_at_unix_ms: Some(observed_at_unix_ms) }
            } else {
                OfficialEndpointStatus::unavailable()
            }
        };
        self.official_snapshot_with_status(
            endpoint(!self.account.fields.is_empty() || self.account.requires_openai_auth.is_some()),
            endpoint(
                self.rate_limits.primary.is_some()
                    || self.rate_limits.secondary.is_some()
                    || !self.rate_limits.raw.is_empty()
                    || self.rate_limits.reset_credits_raw.is_some(),
            ),
            endpoint(self.account_usage.is_some()),
        )
    }

    /// 使用三个端点各自的成功时间和新鲜度生成官方快照。
    #[must_use]
    pub fn official_snapshot_with_status(
        &self,
        account_status: OfficialEndpointStatus,
        quota_status: OfficialEndpointStatus,
        usage_status: OfficialEndpointStatus,
    ) -> OfficialSnapshot {
        let account = (!self.account.fields.is_empty() || self.account.requires_openai_auth.is_some()).then(|| {
            let kind = match string_field(&self.account.fields, &["type"]).unwrap_or_default() {
                "chatgpt" => OfficialAccountKind::ChatGpt,
                "apiKey" | "apikey" => OfficialAccountKind::ApiKey,
                "amazonBedrock" => OfficialAccountKind::AmazonBedrock,
                _ => OfficialAccountKind::Unknown,
            };
            OfficialAccount {
                kind,
                masked_identifier: string_field(&self.account.fields, &["email"]).map(mask_identifier),
                plan_type: string_field(&self.account.fields, &["planType", "plan_type"]).map(str::to_owned),
                auth_mode: string_field(&self.account.fields, &["authMode", "auth_mode"]).map(str::to_owned),
                requires_openai_auth: self.account.requires_openai_auth,
            }
        });
        let credits = self.rate_limits.raw.get("credits").and_then(parse_credits);
        let individual_limit = self.rate_limits.raw.get("individualLimit").and_then(parse_spend_control);
        let spend_control_reached = self
            .rate_limits
            .raw
            .get("spendControlReached")
            .or_else(|| self.rate_limits.raw.get("spend_control_reached"))
            .and_then(Value::as_bool);
        let reset_credits = self.rate_limits.reset_credits_raw.as_ref().and_then(parse_reset_credits);
        let account_usage = self.account_usage.as_ref().and_then(parse_account_usage);
        let mut snapshot = OfficialSnapshot {
            account,
            credits,
            individual_limit,
            spend_control_reached,
            reset_credits,
            account_usage,
            rate_limit_name: string_field(&self.rate_limits.raw, &["limitName", "limit_name"]).map(str::to_owned),
            rate_limit_reached_type: string_field(
                &self.rate_limits.raw,
                &["rateLimitReachedType", "rate_limit_reached_type"],
            )
            .map(str::to_owned),
            account_status,
            quota_status,
            usage_status,
            freshness: OfficialFreshness::Unavailable,
            observed_at_unix_ms: 0,
        };
        snapshot.refresh_aggregate_freshness();
        snapshot
    }
}

/// 合并兼容 token 用量结构；新字段只会进入 raw，不会破坏已解析字段。
pub fn merge_token_usage(existing: &mut Option<TokenUsageUpdate>, update: &Value) {
    let outer_thread_id = update
        .get("params")
        .and_then(|params| params.get("threadId").or_else(|| params.get("thread_id")))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            update.get("threadId").or_else(|| update.get("thread_id")).and_then(Value::as_str).map(str::to_owned)
        });
    let object = nested_object(update, "tokenUsage")
        .or_else(|| nested_object(update, "token_usage"))
        // 部分 App Server 版本直接在 params 中发送 total_token_usage /
        // last_token_usage。这是来自本机 Codex session JSONL 的已确认字段，
        // 不是把任意未知对象猜成 Token 明细。
        .or_else(|| update.get("params").and_then(Value::as_object))
        .or_else(|| update.as_object());
    let Some(object) = object else { return };
    let current = existing.get_or_insert_with(TokenUsageUpdate::default);

    if outer_thread_id.is_some() {
        current.thread_id = outer_thread_id;
    }

    if let Some(value) = object.get("threadId").or_else(|| object.get("thread_id")).and_then(Value::as_str) {
        current.thread_id = Some(value.to_owned());
    }
    // App Server 版本分别使用 total、totalTokenUsage 和 total_token_usage。
    // 按“外层 -> usage -> total”的顺序覆盖，最后的累计明细优先级最高。
    merge_token_counts(current, object);
    if let Some(usage) = object.get("usage").and_then(Value::as_object) {
        merge_token_counts(current, usage);
    }
    if let Some(total) = object
        .get("total")
        .or_else(|| object.get("totalTokenUsage"))
        .or_else(|| object.get("total_token_usage"))
        .and_then(Value::as_object)
    {
        merge_token_counts(current, total);
    }
    if let Some(last) = object
        .get("last")
        .or_else(|| object.get("lastTokenUsage"))
        .or_else(|| object.get("last_token_usage"))
        .and_then(Value::as_object)
    {
        let counts = current.last.get_or_insert_with(TokenCounts::default);
        merge_domain_token_counts(counts, last);
    }
    merge_number(&mut current.model_context_window, object, &["modelContextWindow", "model_context_window"]);
    let mut raw = match &current.raw {
        Value::Object(raw) => raw.clone(),
        _ => Map::new(),
    };
    merge_object(&mut raw, object);
    current.raw = Value::Object(raw);
}

fn merge_token_counts(current: &mut TokenUsageUpdate, object: &Map<String, Value>) {
    merge_number(&mut current.input_tokens, object, &["inputTokens", "input_tokens"]);
    merge_number(&mut current.cached_input_tokens, object, &["cachedInputTokens", "cached_input_tokens"]);
    merge_number(&mut current.cache_write_input_tokens, object, &["cacheWriteInputTokens", "cache_write_input_tokens"]);
    merge_number(&mut current.output_tokens, object, &["outputTokens", "output_tokens"]);
    merge_number(&mut current.reasoning_output_tokens, object, &["reasoningOutputTokens", "reasoning_output_tokens"]);
    merge_number(&mut current.total_tokens, object, &["totalTokens", "total_tokens"]);
}

/// 把当前协议的 TokenUsageBreakdown patch 合并到领域计数，缺失字段沿用上次值。
fn merge_domain_token_counts(current: &mut TokenCounts, object: &Map<String, Value>) {
    merge_number(&mut current.input, object, &["inputTokens", "input_tokens"]);
    merge_number(&mut current.cached_input, object, &["cachedInputTokens", "cached_input_tokens"]);
    merge_number(&mut current.cache_write_input, object, &["cacheWriteInputTokens", "cache_write_input_tokens"]);
    merge_number(&mut current.output, object, &["outputTokens", "output_tokens"]);
    merge_number(&mut current.reasoning_output, object, &["reasoningOutputTokens", "reasoning_output_tokens"]);
    merge_number(&mut current.total, object, &["totalTokens", "total_tokens"]);
}

fn preferred_rate_limit_object(response: &Value) -> Option<&Map<String, Value>> {
    let payload = payload_object(response)?;
    let by_id = payload
        .get("rateLimitsByLimitId")
        .or_else(|| payload.get("rate_limits_by_limit_id"))
        .and_then(Value::as_object);
    if let Some(by_id) = by_id {
        if let Some(codex) = by_id.get("codex").and_then(Value::as_object) {
            return Some(codex);
        }
        if let Some(codex) = by_id.values().filter_map(Value::as_object).find(|snapshot| {
            string_field(snapshot, &["limitId", "limit_id"]).is_some_and(|value| value.eq_ignore_ascii_case("codex"))
        }) {
            return Some(codex);
        }
    }
    payload.get("rateLimits").or_else(|| payload.get("rate_limits")).and_then(Value::as_object)
}

fn payload_object(value: &Value) -> Option<&Map<String, Value>> {
    let object = value.as_object()?;
    for key in ["result", "params"] {
        if let Some(child) = object.get(key).and_then(Value::as_object) {
            return Some(child);
        }
    }
    Some(object)
}

fn find_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key) {
            return Some(value);
        }
    }
    for wrapper in ["result", "params"] {
        if let Some(value) = object.get(wrapper).and_then(|child| find_value(child, keys)) {
            return Some(value);
        }
    }
    None
}

fn string_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| object.get(*key).and_then(Value::as_str)).filter(|value| !value.trim().is_empty())
}

fn parse_credits(value: &Value) -> Option<OfficialCredits> {
    let object = value.as_object()?;
    Some(OfficialCredits {
        has_credits: object.get("hasCredits").or_else(|| object.get("has_credits")).and_then(Value::as_bool)?,
        unlimited: object.get("unlimited").and_then(Value::as_bool).unwrap_or(false),
        balance: object.get("balance").and_then(Value::as_str).map(str::to_owned),
    })
}

fn parse_spend_control(value: &Value) -> Option<OfficialSpendControl> {
    let object = value.as_object()?;
    Some(OfficialSpendControl {
        limit: string_field(object, &["limit"]).map(str::to_owned),
        used: string_field(object, &["used"]).map(str::to_owned),
        remaining_percent: object
            .get("remainingPercent")
            .or_else(|| object.get("remaining_percent"))
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        resets_at_unix: object.get("resetsAt").or_else(|| object.get("resets_at")).and_then(number_i64),
    })
}

fn parse_reset_credits(value: &Value) -> Option<OfficialResetCredits> {
    let object = value.as_object()?;
    let available_count =
        object.get("availableCount").or_else(|| object.get("available_count")).and_then(number_u64)?;
    let mut expiry_times_unix = object
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|credit| credit.get("expiresAt").or_else(|| credit.get("expires_at")).and_then(number_i64))
        .collect::<Vec<_>>();
    expiry_times_unix.sort_unstable();
    expiry_times_unix.dedup();
    let nearest_expiry_unix = expiry_times_unix.first().copied();
    Some(OfficialResetCredits { available_count, expiry_times_unix, nearest_expiry_unix })
}

fn parse_account_usage(snapshot: &AccountUsageSnapshot) -> Option<OfficialAccountUsage> {
    let object = snapshot.raw.as_object()?;
    let summary = object.get("summary").and_then(Value::as_object);
    let daily_usage =
        parse_daily_usage_buckets(object.get("dailyUsageBuckets").or_else(|| object.get("daily_usage_buckets")));
    let thread_usage = parse_thread_usage(object.get("threadUsage").or_else(|| object.get("thread_usage")));
    // 某些 App Server 版本只返回日桶而没有 summary；只要至少有一组可信数据，
    // 仍然投影出官方用量，避免丢掉可以用于趋势图的历史点。
    if summary.is_none() && daily_usage.is_empty() && thread_usage.is_none() {
        return None;
    }
    let latest_bucket = daily_usage.last().map(|bucket| (bucket.date.clone(), bucket.tokens));
    let summary_value = |keys: &[&str]| summary.and_then(|value| numeric_field(value, keys));
    Some(OfficialAccountUsage {
        lifetime_tokens: summary_value(&["lifetimeTokens", "lifetime_tokens"]),
        peak_daily_tokens: summary_value(&["peakDailyTokens", "peak_daily_tokens"]),
        longest_running_turn_seconds: summary_value(&["longestRunningTurnSec", "longest_running_turn_sec"]),
        current_streak_days: summary_value(&["currentStreakDays", "current_streak_days"]),
        longest_streak_days: summary_value(&["longestStreakDays", "longest_streak_days"]),
        daily_usage,
        latest_daily_date: latest_bucket.as_ref().map(|(date, _)| date.clone()),
        latest_daily_tokens: latest_bucket.map(|(_, tokens)| tokens),
        thread_usage,
    })
}

/// 解析 `GetAccountTokenUsageResponse.threadUsage`。
///
/// 仅接受 schema 要求的 threadId 与 estimatedUsageCreditsMicros；USD 是可选
/// 估算，缺失时保留 None。分组缺少其必填 Credits 估算时直接忽略，不能把
/// 不完整对象伪造成可计费模型明细。
fn parse_thread_usage(value: Option<&Value>) -> Option<OfficialThreadUsage> {
    let object = value.and_then(Value::as_object)?;
    let thread_id =
        string_field(object, &["threadId", "thread_id"]).map(str::trim).filter(|value| !value.is_empty())?.to_owned();
    let estimated_usage_credits_micros =
        numeric_field(object, &["estimatedUsageCreditsMicros", "estimated_usage_credits_micros"])?;
    let groups = object
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_thread_usage_group)
        .collect();
    Some(OfficialThreadUsage {
        thread_id,
        estimated_usage_usd_micros: numeric_field(object, &["estimatedUsageUsdMicros", "estimated_usage_usd_micros"]),
        estimated_usage_credits_micros,
        groups,
    })
}

/// 解析 threadUsage.groups 的单个模型分组；遵从 schema 的必填 Credits 估算。
fn parse_thread_usage_group(value: &Value) -> Option<OfficialThreadUsageGroup> {
    let object = value.as_object()?;
    Some(OfficialThreadUsageGroup {
        model: string_field(object, &["model"]).map(str::to_owned),
        input_tokens: numeric_field(object, &["inputTokens", "input_tokens"]),
        cached_input_tokens: numeric_field(object, &["cachedInputTokens", "cached_input_tokens"]),
        net_new_input_tokens: numeric_field(object, &["netNewInputTokens", "net_new_input_tokens"]),
        output_tokens: numeric_field(object, &["outputTokens", "output_tokens"]),
        total_tokens: numeric_field(object, &["totalTokens", "total_tokens"]),
        reasoning_effort: string_field(object, &["reasoningEffort", "reasoning_effort"]).map(str::to_owned),
        speed: string_field(object, &["speed"]).map(str::to_owned),
        estimated_usage_credits_micros: numeric_field(
            object,
            &["estimatedUsageCreditsMicros", "estimated_usage_credits_micros"],
        )?,
    })
}

/// 解析并规范化服务端的 `dailyUsageBuckets`。
///
/// 服务端历史版本曾返回 snake_case，且单项异常不应使整个账户详情失效。因此
/// 这里采用“逐项容错”：只接受严格的公历日期和非负整数 Token，按日期去重，
/// 最终只保留最近 90 个桶，避免异常响应造成无界内存增长。
fn parse_daily_usage_buckets(value: Option<&Value>) -> Vec<OfficialDailyUsage> {
    const MAX_DAILY_USAGE_BUCKETS: usize = 90;
    let Some(buckets) = value.and_then(Value::as_array) else { return Vec::new() };
    let mut parsed = std::collections::BTreeMap::<String, u64>::new();
    for bucket in buckets {
        let Some(object) = bucket.as_object() else { continue };
        let Some(date) = string_field(object, &["startDate", "start_date", "date"]).map(str::trim) else {
            continue;
        };
        if !is_valid_iso_date(date) {
            continue;
        }
        let Some(tokens) = numeric_field(object, &["tokens", "tokenCount", "token_count"]) else { continue };
        // 同一日期出现多次时采用最后一个有效桶，避免把重复通知相加造成虚高。
        parsed.insert(date.to_owned(), tokens);
    }
    let skip = parsed.len().saturating_sub(MAX_DAILY_USAGE_BUCKETS);
    parsed.into_iter().skip(skip).map(|(date, tokens)| OfficialDailyUsage { date, tokens }).collect()
}

/// 只接受 `YYYY-MM-DD`，并校验月份、日期范围（含闰年）。
fn is_valid_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes.iter().enumerate().all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit()) {
        return false;
    }
    let year = value[0..4].parse::<u32>().ok();
    let month = value[5..7].parse::<u32>().ok();
    let day = value[8..10].parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else { return false };
    if year == 0 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 400 == 0 || (year % 4 == 0 && year % 100 != 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days_in_month).contains(&day)
}

fn numeric_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| object.get(*key).and_then(number_u64))
}

fn number_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn mask_identifier(value: &str) -> String {
    let Some((local, domain)) = value.split_once('@') else {
        let visible = value.chars().take(2).collect::<String>();
        return if visible.is_empty() { "***".to_owned() } else { format!("{visible}***") };
    };
    let visible = local.chars().take(2).collect::<String>();
    format!("{}***@{domain}", if visible.is_empty() { "*" } else { &visible })
}

fn nested_object<'a>(value: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    if let Some(child) = object.get("params") {
        if let Some(found) = nested_object(child, key) {
            return Some(found);
        }
    }
    if let Some(child) = object.get(key) {
        return child.as_object();
    }
    // account/rateLimits updated 可能直接把 patch 放在 params 中。
    if key == "account" && (object.contains_key("email") || object.contains_key("planType")) {
        return Some(object);
    }
    if key == "rateLimits" && (object.contains_key("primary") || object.contains_key("secondary")) {
        return Some(object);
    }
    None
}

fn merge_object(existing: &mut Map<String, Value>, update: &Map<String, Value>) {
    for (key, value) in update {
        if value.is_null() {
            // 重要兼容规则：稀疏 null 不是 tombstone。只有完整读取结果才表达 absent。
            continue;
        }
        match (existing.get_mut(key), value) {
            (Some(Value::Object(old)), Value::Object(new)) => merge_object(old, new),
            _ => {
                existing.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_rate_limit_window(existing: &mut Option<RateLimitWindow>, value: Option<&Value>) {
    let Some(value) = value else { return };
    let Some(object) = value.as_object() else { return };
    if value.is_null() {
        return;
    }
    let window = existing.get_or_insert_with(|| RateLimitWindow {
        used_percent: 0.0,
        window_duration_mins: 0,
        resets_at_unix: None,
    });
    if let Some(number) = object.get("usedPercent").or_else(|| object.get("used_percent")).and_then(Value::as_f64) {
        window.used_percent = number as f32;
    }
    if let Some(number) = object
        .get("windowDurationMins")
        .or_else(|| object.get("window_duration_mins"))
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
    {
        window.window_duration_mins = number;
    }
    if let Some(number) = object
        .get("resetsAt")
        .or_else(|| object.get("resets_at"))
        .or_else(|| object.get("resetsAtUnix"))
        .or_else(|| object.get("resets_at_unix"))
        .and_then(number_i64)
    {
        window.resets_at_unix = Some(number);
    }
}

fn merge_number(target: &mut Option<u64>, object: &Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(number) = object.get(*key).and_then(Value::as_u64) {
            *target = Some(number);
            break;
        }
    }
}

fn number_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_account_null_does_not_clear_existing_field() {
        let mut state = AccountSnapshot::default();
        merge_account(
            &mut state,
            &serde_json::json!({"params":{"account":{"email":"a@example.com","planType":"pro"}}}),
        );
        merge_account(&mut state, &serde_json::json!({"params":{"account":{"email":null,"newField":"kept"}}}));
        assert_eq!(state.get("email").and_then(Value::as_str), Some("a@example.com"));
        assert_eq!(state.get("newField").and_then(Value::as_str), Some("kept"));
    }

    #[test]
    fn sparse_rate_limits_patch_keeps_window_fields_and_unknown_data() {
        let mut state = RateLimitsSnapshot::default();
        merge_rate_limits(
            &mut state,
            &serde_json::json!({"params":{"rateLimits":{"primary":{"usedPercent":20,"windowDurationMins":300,"resetsAt":9},"future":{"value":1}}}}),
        );
        merge_rate_limits(
            &mut state,
            &serde_json::json!({"params":{"rateLimits":{"primary":{"usedPercent":null,"resetsAt":null},"future":null}}}),
        );
        assert_eq!(state.primary.as_ref().unwrap().used_percent, 20.0);
        assert_eq!(state.primary.as_ref().unwrap().resets_at_unix, Some(9));
        assert_eq!(state.raw["future"]["value"], 1);
    }

    #[test]
    fn full_weekly_only_read_clears_previous_five_hour_window() {
        let mut limits = RateLimitsSnapshot::default();
        replace_rate_limits(
            &mut limits,
            &serde_json::json!({
                "rateLimits": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 300},
                    "secondary": {"usedPercent": 20, "windowDurationMins": 10080}
                }
            }),
        );
        replace_rate_limits(
            &mut limits,
            &serde_json::json!({
                "rateLimits": {
                    "primary": null,
                    "secondary": {"usedPercent": 21, "windowDurationMins": 10080}
                }
            }),
        );

        let snapshot = limits.snapshot(100, 2);
        assert!(snapshot.five_hour.is_none());
        assert_eq!(snapshot.weekly.map(|value| value.used_percent), Some(21.0));
    }

    #[test]
    fn token_usage_keeps_unknown_fields() {
        let mut usage = None;
        merge_token_usage(
            &mut usage,
            &serde_json::json!({"params":{"tokenUsage":{"threadId":"t1","inputTokens":3,"cacheReadTokens":8}}}),
        );
        let usage = usage.unwrap();
        assert_eq!(usage.thread_id.as_deref(), Some("t1"));
        assert_eq!(usage.input_tokens, Some(3));
        assert_eq!(usage.raw["cacheReadTokens"], 8);
    }

    #[test]
    fn current_schema_token_usage_reads_cumulative_total_breakdown() {
        let mut usage = None;
        merge_token_usage(
            &mut usage,
            &serde_json::json!({
                "params": {
                    "threadId": "thread-current",
                    "turnId": "turn-current",
                    "tokenUsage": {
                        "last": {
                            "inputTokens": 3,
                            "cachedInputTokens": 2,
                            "cacheWriteInputTokens": 1,
                            "outputTokens": 1,
                            "reasoningOutputTokens": 1,
                            "totalTokens": 4
                        },
                        "total": {
                            "inputTokens": 30,
                            "cachedInputTokens": 20,
                            "cacheWriteInputTokens": 8,
                            "outputTokens": 10,
                            "reasoningOutputTokens": 7,
                            "totalTokens": 40
                        },
                        "modelContextWindow": 200000
                    }
                }
            }),
        );

        let usage = usage.expect("usage");
        assert_eq!(usage.thread_id.as_deref(), Some("thread-current"));
        assert_eq!(usage.input_tokens, Some(30));
        assert_eq!(usage.cached_input_tokens, Some(20));
        assert_eq!(usage.cache_write_input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(10));
        assert_eq!(usage.reasoning_output_tokens, Some(7));
        assert_eq!(usage.total_tokens, Some(40));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.input), Some(3));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.cached_input), Some(2));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.cache_write_input), Some(1));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.output), Some(1));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.reasoning_output), Some(1));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.total), Some(4));
        assert_eq!(usage.model_context_window, Some(200_000));
        assert_eq!(usage.raw["modelContextWindow"], 200000);
    }

    #[test]
    fn snake_case_token_usage_aliases_from_local_codex_sessions_are_mapped() {
        let mut usage = None;
        // 该字段形状来自本机 Codex session JSONL 的 token_count 事件；App Server
        // 在兼容模式下会以同名 snake_case 明细封装到 thread token 通知中。
        merge_token_usage(
            &mut usage,
            &serde_json::json!({
                "params": {
                    "thread_id": "thread-local",
                    "total_token_usage": {
                        "input_tokens": 300,
                        "cached_input_tokens": 200,
                        "cache_write_input_tokens": 12,
                        "output_tokens": 40,
                        "reasoning_output_tokens": 8,
                        "total_tokens": 340
                    },
                    "last_token_usage": {
                        "input_tokens": 30,
                        "cached_input_tokens": 20,
                        "output_tokens": 4,
                        "reasoning_output_tokens": 1,
                        "total_tokens": 34
                    },
                    "model_context_window": 258400
                }
            }),
        );

        let usage = usage.expect("usage");
        assert_eq!(usage.thread_id.as_deref(), Some("thread-local"));
        assert_eq!(usage.input_tokens, Some(300));
        assert_eq!(usage.cached_input_tokens, Some(200));
        assert_eq!(usage.cache_write_input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.reasoning_output_tokens, Some(8));
        assert_eq!(usage.total_tokens, Some(340));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.total), Some(34));
        assert_eq!(usage.model_context_window, Some(258_400));
    }

    #[test]
    fn parallel_threads_are_aggregated_by_attention_priority() {
        let mut state = AppServerState::default();
        state.apply_notification_at(
            &serde_json::json!({"method":"item/started","params":{"threadId":"executing","type":"command"}}),
            1_000,
        );
        state.apply_notification_at(
            &serde_json::json!({"method":"requestUserInput/requested","params":{"threadId":"waiting"}}),
            1_100,
        );

        assert_eq!(
            state.aggregated_activity(1_200).map(|event| event.state),
            Some(codex_taskbar_domain::activity::ActivityState::WaitingForUser)
        );
    }

    #[test]
    fn unscoped_activity_cannot_override_known_thread_collection() {
        let mut state = AppServerState::default();
        state.apply_notification_at(&serde_json::json!({"method":"turn/started","params":{"threadId":"known"}}), 1_000);
        state.apply_notification_at(&serde_json::json!({"method":"turn/completed","params":{}}), 1_100);

        assert_eq!(
            state.aggregated_activity(1_200).map(|event| event.state),
            Some(codex_taskbar_domain::activity::ActivityState::Thinking)
        );
    }

    #[test]
    fn old_terminal_thread_does_not_hide_a_running_thread() {
        let mut state = AppServerState::default();
        state.apply_notification_at(&serde_json::json!({"method":"turn/failed","params":{"threadId":"failed"}}), 1_000);
        state.apply_notification_at(
            &serde_json::json!({"method":"item/started","params":{"threadId":"running","type":"command"}}),
            2_000,
        );

        assert_eq!(
            state.aggregated_activity(2_100).map(|event| event.state),
            Some(codex_taskbar_domain::activity::ActivityState::Failed)
        );
        assert_eq!(
            state.aggregated_activity(12_001).map(|event| event.state),
            Some(codex_taskbar_domain::activity::ActivityState::Executing)
        );
    }

    #[test]
    fn sparse_current_schema_update_keeps_last_and_context_fields() {
        let mut usage = None;
        merge_token_usage(
            &mut usage,
            &serde_json::json!({
                "params": {"tokenUsage": {
                    "last": {"inputTokens": 3, "cacheWriteInputTokens": 1},
                    "total": {"inputTokens": 30, "cacheWriteInputTokens": 8},
                    "modelContextWindow": 200000
                }}
            }),
        );
        merge_token_usage(
            &mut usage,
            &serde_json::json!({
                "params": {"tokenUsage": {
                    "last": {"outputTokens": 2},
                    "total": {"outputTokens": 12}
                }}
            }),
        );

        let usage = usage.expect("usage");
        assert_eq!(usage.input_tokens, Some(30));
        assert_eq!(usage.cache_write_input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(12));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.input), Some(3));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.cache_write_input), Some(1));
        assert_eq!(usage.last.as_ref().and_then(|counts| counts.output), Some(2));
        assert_eq!(usage.model_context_window, Some(200_000));
    }

    #[test]
    fn full_account_read_replaces_identity_and_masks_email_in_domain_snapshot() {
        let mut state = AppServerState::default();
        replace_account(
            &mut state.account,
            &serde_json::json!({
                "account": {"type":"chatgpt","email":"private@example.com","planType":"pro"},
                "requiresOpenaiAuth": true
            }),
        );
        let snapshot = state.official_snapshot(OfficialFreshness::Live, 10);
        let account = snapshot.account.expect("account");
        assert_eq!(account.masked_identifier.as_deref(), Some("pr***@example.com"));
        assert_eq!(account.plan_type.as_deref(), Some("pro"));

        replace_account(
            &mut state.account,
            &serde_json::json!({"account":{"type":"apiKey"},"requiresOpenaiAuth":false}),
        );
        assert_eq!(state.account.get("email"), None);
        assert_eq!(
            state.official_snapshot(OfficialFreshness::Live, 20).account.map(|value| value.kind),
            Some(OfficialAccountKind::ApiKey)
        );
    }

    #[test]
    fn multi_bucket_codex_limits_override_legacy_and_parse_extended_fields() {
        let mut state = AppServerState::default();
        replace_rate_limits(
            &mut state.rate_limits,
            &serde_json::json!({
                "rateLimits": {"primary":{"usedPercent":99,"windowDurationMins":300}},
                "rateLimitsByLimitId": {
                    "other": {"primary":{"usedPercent":88,"windowDurationMins":300}},
                    "codex": {
                        "limitId":"codex",
                        "limitName":"Codex",
                        "primary":{"usedPercent":25,"windowDurationMins":300},
                        "secondary":{"usedPercent":40,"windowDurationMins":10080},
                        "credits":{"hasCredits":true,"unlimited":false,"balance":"8.50"},
                        "individualLimit":{"limit":"50","used":"10","remainingPercent":80,"resetsAt":42},
                        "spendControlReached":false
                    }
                },
                "rateLimitResetCredits": {
                    "availableCount":2,
                    "credits":[{"expiresAt":100},{"expiresAt":80}]
                }
            }),
        );
        let quota = state.rate_limits.snapshot(1, 1);
        assert_eq!(quota.five_hour.map(|value| value.used_percent), Some(25.0));
        assert_eq!(quota.weekly.map(|value| value.used_percent), Some(40.0));
        let official = state.official_snapshot(OfficialFreshness::Live, 1);
        assert_eq!(official.credits.and_then(|value| value.balance), Some("8.50".to_owned()));
        let reset = official.reset_credits.expect("重置卡");
        assert_eq!(reset.available_count, 2);
        assert_eq!(reset.expiry_times_unix, vec![80, 100]);
        assert_eq!(reset.nearest_expiry_unix, Some(80));
        assert_eq!(official.individual_limit.and_then(|value| value.remaining_percent), Some(80.0));
    }

    #[test]
    fn account_usage_is_separate_from_thread_usage_and_keeps_latest_bucket_label() {
        let mut state = AppServerState::default();
        replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({
                "summary": {
                    "lifetimeTokens":"1200000",
                    "peakDailyTokens":450000,
                    "currentStreakDays":7
                },
                "dailyUsageBuckets": [
                    {"startDate":"2026-08-21","tokens":10},
                    {"startDate":"2026-08-22","tokens":"20"}
                ]
            }),
        );
        assert!(state.token_usage.is_none());
        let usage = state.official_snapshot(OfficialFreshness::Live, 1).account_usage.expect("official account usage");
        assert_eq!(usage.lifetime_tokens, Some(1_200_000));
        assert_eq!(usage.daily_usage.len(), 2);
        assert_eq!(usage.daily_usage[0].date, "2026-08-21");
        assert_eq!(usage.daily_usage[0].tokens, 10);
        assert_eq!(usage.daily_usage[1].date, "2026-08-22");
        assert_eq!(usage.daily_usage[1].tokens, 20);
        assert_eq!(usage.latest_daily_date.as_deref(), Some("2026-08-22"));
        assert_eq!(usage.latest_daily_tokens, Some(20));
    }

    #[test]
    fn account_token_usage_schema_maps_server_estimates_and_model_groups() {
        let mut state = AppServerState::default();
        // 对应 codex-cli 0.149.0-alpha.4.1 的
        // GetAccountTokenUsageResponse.json：金额完全来自服务端估算字段。
        assert!(replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({
                "summary": {"lifetimeTokens": 1234},
                "threadUsage": {
                    "threadId": "thread-current",
                    "estimatedUsageUsdMicros": 1_250,
                    "estimatedUsageCreditsMicros": 3_000,
                    "groups": [
                        {
                            "model": "gpt-5.6",
                            "inputTokens": 800,
                            "cachedInputTokens": 600,
                            "netNewInputTokens": 200,
                            "outputTokens": 45,
                            "totalTokens": 845,
                            "reasoningEffort": "high",
                            "speed": "fast",
                            "estimatedUsageCreditsMicros": 2_400
                        },
                        {
                            "model": null,
                            "cached_input_tokens": 50,
                            "output_tokens": 5,
                            "estimated_usage_credits_micros": 600
                        },
                        // 当前 schema 要求 Credits；不完整组必须被跳过。
                        {"model": "incomplete", "inputTokens": 1}
                    ]
                }
            })
        ));

        let usage = state.official_snapshot(OfficialFreshness::Live, 1).account_usage.expect("official usage");
        let thread = usage.thread_usage.expect("thread estimate");
        assert_eq!(thread.thread_id, "thread-current");
        assert_eq!(thread.estimated_usage_usd_micros, Some(1_250));
        assert_eq!(thread.estimated_usage_credits_micros, 3_000);
        assert_eq!(thread.groups.len(), 2);
        assert_eq!(thread.groups[0].model.as_deref(), Some("gpt-5.6"));
        assert_eq!(thread.groups[0].input_tokens, Some(800));
        assert_eq!(thread.groups[0].cached_input_tokens, Some(600));
        assert_eq!(thread.groups[0].output_tokens, Some(45));
        assert_eq!(thread.groups[0].estimated_usage_credits_micros, 2_400);
        assert_eq!(thread.groups[1].cached_input_tokens, Some(50));
        assert_eq!(thread.groups[1].estimated_usage_credits_micros, 600);
    }

    #[test]
    fn missing_thread_usage_estimate_stays_absent_instead_of_becoming_zero() {
        let mut state = AppServerState::default();
        assert!(replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({
                "summary": {},
                "threadUsage": {
                    "threadId": "thread-current",
                    "groups": []
                }
            })
        ));

        let usage = state.official_snapshot(OfficialFreshness::Live, 1).account_usage.expect("summary is valid");
        assert!(usage.thread_usage.is_none());
    }

    #[test]
    fn daily_usage_buckets_are_sorted_deduplicated_and_invalid_rows_ignored() {
        let mut state = AppServerState::default();
        replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({
                "dailyUsageBuckets": [
                    {"startDate":"2026-02-30","tokens":999},
                    {"startDate":"2026-08-23T00:00:00Z","tokens":998},
                    {"startDate":"2026-08-22","tokens":"20"},
                    {"start_date":"2026-08-21","tokens":10},
                    {"date":"2026-08-22","tokens":25},
                    {"startDate":"2026-08-20","tokens":null},
                    "not-an-object"
                ]
            }),
        );
        let usage = state.official_snapshot(OfficialFreshness::Live, 1).account_usage.expect("daily usage");
        assert_eq!(
            usage.daily_usage,
            vec![
                OfficialDailyUsage { date: "2026-08-21".to_owned(), tokens: 10 },
                OfficialDailyUsage { date: "2026-08-22".to_owned(), tokens: 25 },
            ]
        );
        assert_eq!(usage.latest_daily_date.as_deref(), Some("2026-08-22"));
        assert_eq!(usage.latest_daily_tokens, Some(25));
    }

    #[test]
    fn daily_usage_buckets_keep_only_the_latest_ninety_dates() {
        let buckets = (0..100)
            .map(|offset| {
                let year = 2000 + offset;
                serde_json::json!({"startDate": format!("{year:04}-01-01"), "tokens": offset})
            })
            .collect::<Vec<_>>();
        let mut state = AppServerState::default();
        replace_account_usage(&mut state.account_usage, &serde_json::json!({"dailyUsageBuckets": buckets}));
        let usage = state.official_snapshot(OfficialFreshness::Live, 1).account_usage.expect("daily usage");
        assert_eq!(usage.daily_usage.len(), 90);
        assert_eq!(usage.daily_usage.first().map(|bucket| bucket.date.as_str()), Some("2010-01-01"));
        assert_eq!(usage.daily_usage.last().map(|bucket| bucket.date.as_str()), Some("2099-01-01"));
    }

    #[test]
    fn account_usage_with_only_malformed_daily_buckets_is_unavailable() {
        let mut state = AppServerState::default();
        replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({"dailyUsageBuckets": [{"startDate":"yesterday","tokens":1}]}),
        );
        assert!(state.official_snapshot(OfficialFreshness::Live, 1).account_usage.is_none());
    }
}

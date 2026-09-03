//! 不读取会话内容的本机 Token 增量账本。
//!
//! Codex 的 `state_*.sqlite` 只提供每线程累计 `tokens_used`。本模块只将后续
//! 正向差值归入本地日期/小时桶；首帧、新线程和累计回退只重建基线。持久化格式
//! 故意不包含线程标识、路径、标题、提示词或任何 `item_json` 内容。

use std::collections::BTreeMap;

use codex_taskbar_domain::usage::TokenCounts;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// 结构化线程累计的最小投影。调用方不得在日志或持久化文件中输出 `thread_id`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTokenCounter {
    pub thread_id: String,
    pub tokens_used: u64,
}

/// 本机日期和小时，由平台层以用户本地时区提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalUsageClock {
    /// `YYYYMMDD`，例如 `20260827`。
    pub day_key: i32,
    pub hour: u8,
}

/// 供 JSON 持久化的无敏感统计。基线只留在内存；重启后会重新建立，不会把历史累计量
/// 重新计入今天。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedLocalUsageLedger {
    pub version: u8,
    pub days: Vec<PersistedUsageDay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedUsageDay {
    pub day_key: i32,
    pub total_tokens: u64,
    pub hourly_tokens: [u64; 24],
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default = "empty_hourly_tokens")]
    pub hourly_input_tokens: [u64; 24],
    #[serde(default = "empty_hourly_tokens")]
    pub hourly_cached_input_tokens: [u64; 24],
    #[serde(default = "empty_hourly_tokens")]
    pub hourly_output_tokens: [u64; 24],
    /// 按事件模型与官方 API 标准文本单价得到的微美元等价值；不是订阅账单。
    #[serde(default)]
    pub estimated_api_cost_micro_usd: u64,
    #[serde(default = "empty_hourly_tokens")]
    pub hourly_estimated_api_cost_micro_usd: [u64; 24],
}

/// 在内存中维护线程基线，在磁盘上只维护桶总计。
#[derive(Debug)]
pub struct LocalUsageLedger {
    baselines: BTreeMap<String, u64>,
    days: BTreeMap<i32, UsageDay>,
    dirty: bool,
    retained_days: usize,
}

impl Default for LocalUsageLedger {
    fn default() -> Self {
        Self { baselines: BTreeMap::new(), days: BTreeMap::new(), dirty: false, retained_days: 90 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageDay {
    total_tokens: u64,
    hourly_tokens: [u64; 24],
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    hourly_input_tokens: [u64; 24],
    hourly_cached_input_tokens: [u64; 24],
    hourly_output_tokens: [u64; 24],
    estimated_api_cost_micro_usd: u64,
    hourly_estimated_api_cost_micro_usd: [u64; 24],
}

const fn empty_hourly_tokens() -> [u64; 24] {
    [0; 24]
}

impl UsageDay {
    const fn empty() -> Self {
        Self {
            total_tokens: 0,
            hourly_tokens: [0; 24],
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            hourly_input_tokens: [0; 24],
            hourly_cached_input_tokens: [0; 24],
            hourly_output_tokens: [0; 24],
            estimated_api_cost_micro_usd: 0,
            hourly_estimated_api_cost_micro_usd: [0; 24],
        }
    }

    fn counts(&self) -> TokenCounts {
        TokenCounts {
            input: (self.input_tokens > 0).then_some(self.input_tokens),
            cached_input: (self.cached_input_tokens > 0).then_some(self.cached_input_tokens),
            output: (self.output_tokens > 0).then_some(self.output_tokens),
            total: (self.total_tokens > 0).then_some(self.total_tokens),
            ..TokenCounts::default()
        }
    }

    fn add_counts(&mut self, hour: usize, counts: &TokenCounts, estimated_api_cost_micro_usd: Option<u64>) {
        let total = counts.display_total().unwrap_or(0);
        let input = counts.input.unwrap_or(0);
        let cached = counts.cached_input.unwrap_or(0);
        let output = counts.output.unwrap_or(0);
        self.total_tokens = self.total_tokens.saturating_add(total);
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.cached_input_tokens = self.cached_input_tokens.saturating_add(cached);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.hourly_tokens[hour] = self.hourly_tokens[hour].saturating_add(total);
        self.hourly_input_tokens[hour] = self.hourly_input_tokens[hour].saturating_add(input);
        self.hourly_cached_input_tokens[hour] = self.hourly_cached_input_tokens[hour].saturating_add(cached);
        self.hourly_output_tokens[hour] = self.hourly_output_tokens[hour].saturating_add(output);
        if let Some(cost) = estimated_api_cost_micro_usd {
            self.estimated_api_cost_micro_usd = self.estimated_api_cost_micro_usd.saturating_add(cost);
            self.hourly_estimated_api_cost_micro_usd[hour] =
                self.hourly_estimated_api_cost_micro_usd[hour].saturating_add(cost);
        }
    }
}

impl LocalUsageLedger {
    pub const VERSION: u8 = 3;
    const MAX_RETAINED_DAYS: usize = 365;

    /// 清空仅属于本应用的聚合历史，同时丢弃内存基线，避免清理后把旧累计误算为新增。
    /// 调用方应在用户明确二次确认后执行，并立即持久化空账本。
    pub fn clear(&mut self) {
        self.baselines.clear();
        self.days.clear();
        self.dirty = true;
    }

    #[must_use]
    pub fn from_persisted(persisted: PersistedLocalUsageLedger) -> Self {
        let mut days = persisted
            .days
            .into_iter()
            .map(|day| {
                (day.day_key, {
                    let mut hourly_tokens = day.hourly_tokens;
                    for (hour, hourly_total) in hourly_tokens.iter_mut().enumerate() {
                        if day.hourly_input_tokens[hour] > 0 || day.hourly_output_tokens[hour] > 0 {
                            *hourly_total =
                                day.hourly_input_tokens[hour].saturating_add(day.hourly_output_tokens[hour]);
                        }
                    }
                    let detailed_total = day.input_tokens.saturating_add(day.output_tokens);
                    UsageDay {
                        total_tokens: if day.input_tokens > 0 || day.output_tokens > 0 {
                            detailed_total
                        } else {
                            day.total_tokens
                        },
                        hourly_tokens,
                        input_tokens: day.input_tokens,
                        cached_input_tokens: day.cached_input_tokens,
                        output_tokens: day.output_tokens,
                        hourly_input_tokens: day.hourly_input_tokens,
                        hourly_cached_input_tokens: day.hourly_cached_input_tokens,
                        hourly_output_tokens: day.hourly_output_tokens,
                        estimated_api_cost_micro_usd: day.estimated_api_cost_micro_usd,
                        hourly_estimated_api_cost_micro_usd: day.hourly_estimated_api_cost_micro_usd,
                    }
                })
            })
            .collect::<BTreeMap<_, _>>();
        while days.len() > Self::MAX_RETAINED_DAYS {
            let Some(first) = days.first_key_value().map(|(key, _)| *key) else { break };
            days.remove(&first);
        }
        Self { baselines: BTreeMap::new(), days, dirty: false, retained_days: 90 }
    }

    #[must_use]
    pub fn persisted(&self) -> PersistedLocalUsageLedger {
        PersistedLocalUsageLedger {
            version: Self::VERSION,
            days: self
                .days
                .iter()
                .map(|(&day_key, day)| PersistedUsageDay {
                    day_key,
                    total_tokens: day.total_tokens,
                    hourly_tokens: day.hourly_tokens,
                    input_tokens: day.input_tokens,
                    cached_input_tokens: day.cached_input_tokens,
                    output_tokens: day.output_tokens,
                    hourly_input_tokens: day.hourly_input_tokens,
                    hourly_cached_input_tokens: day.hourly_cached_input_tokens,
                    hourly_output_tokens: day.hourly_output_tokens,
                    estimated_api_cost_micro_usd: day.estimated_api_cost_micro_usd,
                    hourly_estimated_api_cost_micro_usd: day.hourly_estimated_api_cost_micro_usd,
                })
                .collect(),
        }
    }

    /// 从关系型日/小时桶读取本机账本。数据库只保存聚合数字，不保存线程、Prompt
    /// 或回复；线程基线仍只存在于进程内存中。
    pub fn load_sqlite(path: &std::path::Path) -> rusqlite::Result<Self> {
        let connection = open_usage_database(path)?;
        let mut days = BTreeMap::new();
        let mut repaired = false;
        {
            let mut statement = connection.prepare(
                "SELECT day_key,total_tokens,input_tokens,cached_input_tokens,output_tokens,estimated_api_cost_micro_usd FROM usage_daily ORDER BY day_key",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                ))
            })?;
            for row in rows {
                let (day_key, total, input, cached, output, cost) = row?;
                let canonical_total = if input > 0 || output > 0 { input.saturating_add(output) } else { total };
                repaired |= canonical_total != total || cached > input;
                days.insert(
                    day_key,
                    UsageDay {
                        total_tokens: canonical_total,
                        input_tokens: input,
                        cached_input_tokens: cached.min(input),
                        output_tokens: output,
                        estimated_api_cost_micro_usd: cost,
                        ..UsageDay::empty()
                    },
                );
            }
        }
        {
            let mut statement = connection.prepare(
                "SELECT day_key,hour,total_tokens,input_tokens,cached_input_tokens,output_tokens,estimated_api_cost_micro_usd FROM usage_hourly ORDER BY day_key,hour",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, u8>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                ))
            })?;
            for row in rows {
                let (day_key, hour, total, input, cached, output, cost) = row?;
                let Some(day) = days.get_mut(&day_key) else { continue };
                let index = usize::from(hour.min(23));
                let canonical_total = if input > 0 || output > 0 { input.saturating_add(output) } else { total };
                repaired |= canonical_total != total || cached > input;
                day.hourly_tokens[index] = canonical_total;
                day.hourly_input_tokens[index] = input;
                day.hourly_cached_input_tokens[index] = cached.min(input);
                day.hourly_output_tokens[index] = output;
                day.hourly_estimated_api_cost_micro_usd[index] = cost;
            }
        }
        // 若数据库来自曾重复累计缓存的版本，下一次低频落盘会把修正值回写，
        // 不只是在这一轮 UI 中临时显示正确结果。
        Ok(Self { baselines: BTreeMap::new(), days, dirty: repaired, retained_days: 90 })
    }

    /// 用一个事务替换关系型聚合桶。最多 365×24 行且仅按低频批次调用，避免
    /// 每个 Token 或动画帧触发磁盘 I/O。
    pub fn save_sqlite(&mut self, path: &std::path::Path) -> rusqlite::Result<()> {
        let mut connection = open_usage_database(path)?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM usage_hourly", [])?;
        transaction.execute("DELETE FROM usage_daily", [])?;
        for (&day_key, day) in &self.days {
            transaction.execute(
                "INSERT INTO usage_daily(day_key,total_tokens,input_tokens,cached_input_tokens,output_tokens,estimated_api_cost_micro_usd) VALUES(?1,?2,?3,?4,?5,?6)",
                params![day_key, day.total_tokens, day.input_tokens, day.cached_input_tokens, day.output_tokens, day.estimated_api_cost_micro_usd],
            )?;
            for hour in 0..24 {
                if day.hourly_tokens[hour] == 0
                    && day.hourly_input_tokens[hour] == 0
                    && day.hourly_output_tokens[hour] == 0
                {
                    continue;
                }
                transaction.execute(
                    "INSERT INTO usage_hourly(day_key,hour,total_tokens,input_tokens,cached_input_tokens,output_tokens,estimated_api_cost_micro_usd) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![day_key, hour as u8, day.hourly_tokens[hour], day.hourly_input_tokens[hour], day.hourly_cached_input_tokens[hour], day.hourly_output_tokens[hour], day.hourly_estimated_api_cost_micro_usd[hour]],
                )?;
            }
        }
        transaction.commit()?;
        self.mark_persisted();
        Ok(())
    }

    /// 接纳一帧线程累计量。返回值只代表本地统计是否新增，便于调用方节流落盘。
    pub fn observe(&mut self, clock: LocalUsageClock, counters: impl IntoIterator<Item = ThreadTokenCounter>) -> bool {
        let hour = usize::from(clock.hour.min(23));
        let mut delta = 0_u64;
        for counter in counters {
            let previous = self.baselines.insert(counter.thread_id, counter.tokens_used);
            if let Some(previous) = previous.filter(|previous| counter.tokens_used > *previous) {
                delta = delta.saturating_add(counter.tokens_used.saturating_sub(previous));
            }
        }
        if delta == 0 {
            return false;
        }
        let day = self.days.entry(clock.day_key).or_insert_with(UsageDay::empty);
        day.total_tokens = day.total_tokens.saturating_add(delta);
        day.hourly_tokens[hour] = day.hourly_tokens[hour].saturating_add(delta);
        self.prune_days();
        self.dirty = true;
        true
    }

    /// 用 session JSONL 的完整当日扫描结果替换当天桶。启动时采用替换而不是
    /// 增量相加，因此重启不会重复累计；只保存数值桶，不保存会话内容或事件 ID。
    pub fn replace_session_day(&mut self, day_key: i32, events: impl IntoIterator<Item = (u8, TokenCounts)>) -> bool {
        self.replace_session_day_priced(day_key, events.into_iter().map(|(hour, counts)| (hour, counts, None)))
    }

    /// 与 [`Self::replace_session_day`] 相同，但同时接收已经由应用层按明确模型
    /// 计算完成的 API 等价微美元。账本不认识模型或价格表，只保存聚合数值。
    pub fn replace_session_day_priced(
        &mut self,
        day_key: i32,
        events: impl IntoIterator<Item = (u8, TokenCounts, Option<u64>)>,
    ) -> bool {
        let mut replacement = UsageDay::empty();
        for (hour, counts, estimated_api_cost_micro_usd) in events {
            replacement.add_counts(usize::from(hour.min(23)), &counts, estimated_api_cost_micro_usd);
        }
        if self.days.get(&day_key) == Some(&replacement) {
            return false;
        }
        self.days.insert(day_key, replacement);
        self.prune_days();
        self.dirty = true;
        true
    }

    /// 完整扫描完成后的实时新增事件直接进入同一详细桶。
    pub fn observe_session_event(&mut self, clock: LocalUsageClock, counts: &TokenCounts) -> bool {
        self.observe_session_event_priced(clock, counts, None)
    }

    /// 记录一条带 API 等价估算的 session 事件。金额缺失时仍记录 Token，禁止
    /// 因未知模型而把价格猜成零。
    pub fn observe_session_event_priced(
        &mut self,
        clock: LocalUsageClock,
        counts: &TokenCounts,
        estimated_api_cost_micro_usd: Option<u64>,
    ) -> bool {
        if counts.display_total().unwrap_or(0) == 0 {
            return false;
        }
        self.days.entry(clock.day_key).or_insert_with(UsageDay::empty).add_counts(
            usize::from(clock.hour.min(23)),
            counts,
            estimated_api_cost_micro_usd,
        );
        self.prune_days();
        self.dirty = true;
        true
    }

    #[must_use]
    pub fn today_counts(&self, day_key: i32) -> Option<TokenCounts> {
        self.days.get(&day_key).map(UsageDay::counts).filter(|counts| counts.display_total().is_some())
    }

    #[must_use]
    pub fn today_hourly_tokens(&self, day_key: i32) -> [u64; 24] {
        self.days.get(&day_key).map_or([0; 24], |day| day.hourly_tokens)
    }

    #[must_use]
    pub fn today_estimated_api_cost_micro_usd(&self, day_key: i32) -> Option<u64> {
        self.days.get(&day_key).map(|day| day.estimated_api_cost_micro_usd).filter(|cost| *cost > 0)
    }

    #[must_use]
    pub fn today_hourly_estimated_api_cost_micro_usd(&self, day_key: i32) -> [u64; 24] {
        self.days.get(&day_key).map_or([0; 24], |day| day.hourly_estimated_api_cost_micro_usd)
    }

    /// 返回最近若干个本机日聚合桶，按日期从旧到新排列。
    ///
    /// 返回类型是持久化时同样使用的无敏感聚合投影，不包含内存基线里的线程
    /// 标识；详情趋势可以直接消费它，不需要再次扫描 Codex SQLite。
    #[must_use]
    pub fn recent_days(&self, limit: usize) -> Vec<PersistedUsageDay> {
        self.days
            .iter()
            .rev()
            .take(limit)
            .map(|(&day_key, day)| PersistedUsageDay {
                day_key,
                total_tokens: day.total_tokens,
                hourly_tokens: day.hourly_tokens,
                input_tokens: day.input_tokens,
                cached_input_tokens: day.cached_input_tokens,
                output_tokens: day.output_tokens,
                hourly_input_tokens: day.hourly_input_tokens,
                hourly_cached_input_tokens: day.hourly_cached_input_tokens,
                hourly_output_tokens: day.hourly_output_tokens,
                estimated_api_cost_micro_usd: day.estimated_api_cost_micro_usd,
                hourly_estimated_api_cost_micro_usd: day.hourly_estimated_api_cost_micro_usd,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_persisted(&mut self) {
        self.dirty = false;
    }

    /// 应用设置页的本机历史保留时长。缩短范围时立即只在内存中裁剪，之后仍由
    /// 原有低频批量写盘落地，避免设置操作引入高频磁盘 I/O。
    pub fn set_retention_days(&mut self, retained_days: usize) {
        self.retained_days = retained_days.clamp(1, Self::MAX_RETAINED_DAYS);
        let before = self.days.len();
        self.prune_days();
        self.dirty |= self.days.len() != before;
    }

    fn prune_days(&mut self) {
        while self.days.len() > self.retained_days {
            let Some(first) = self.days.first_key_value().map(|(key, _)| *key) else { break };
            self.days.remove(&first);
        }
    }
}

fn open_usage_database(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS usage_daily(
           day_key INTEGER PRIMARY KEY,
           total_tokens INTEGER NOT NULL,
           input_tokens INTEGER NOT NULL,
           cached_input_tokens INTEGER NOT NULL,
           output_tokens INTEGER NOT NULL,
           estimated_api_cost_micro_usd INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS usage_hourly(
           day_key INTEGER NOT NULL,
           hour INTEGER NOT NULL CHECK(hour BETWEEN 0 AND 23),
           total_tokens INTEGER NOT NULL,
           input_tokens INTEGER NOT NULL,
           cached_input_tokens INTEGER NOT NULL,
           output_tokens INTEGER NOT NULL,
           estimated_api_cost_micro_usd INTEGER NOT NULL,
           PRIMARY KEY(day_key,hour),
           FOREIGN KEY(day_key) REFERENCES usage_daily(day_key) ON DELETE CASCADE
         );",
    )?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter(id: &str, tokens_used: u64) -> ThreadTokenCounter {
        ThreadTokenCounter { thread_id: id.to_owned(), tokens_used }
    }

    #[test]
    fn first_observation_and_counter_regression_only_reset_the_baseline() {
        let mut ledger = LocalUsageLedger::default();
        let clock = LocalUsageClock { day_key: 20260827, hour: 9 };

        assert!(!ledger.observe(clock, [counter("thread-a", 100)]));
        assert!(ledger.observe(clock, [counter("thread-a", 145)]));
        assert!(!ledger.observe(clock, [counter("thread-a", 20)]));
        assert!(ledger.observe(clock, [counter("thread-a", 24)]));
        assert_eq!(ledger.today_counts(clock.day_key).and_then(|counts| counts.total), Some(49));
    }

    #[test]
    fn persists_only_aggregate_buckets_without_thread_identifiers() {
        let mut ledger = LocalUsageLedger::default();
        let first = LocalUsageClock { day_key: 20260827, hour: 9 };
        assert!(!ledger.observe(first, [counter("private-thread", 1)]));
        assert!(ledger.observe(LocalUsageClock { hour: 10, ..first }, [counter("private-thread", 13)]));

        let serialized = serde_json::to_string(&ledger.persisted()).expect("账本应可序列化");
        assert!(!serialized.contains("private-thread"));
        assert!(serialized.contains("12"));
        let restored = LocalUsageLedger::from_persisted(serde_json::from_str(&serialized).expect("账本应可反序列化"));
        assert_eq!(restored.today_counts(first.day_key).and_then(|counts| counts.total), Some(12));
    }

    #[test]
    fn new_day_uses_a_separate_bucket_and_retain_limit_is_bounded() {
        let mut ledger = LocalUsageLedger::default();
        assert!(!ledger.observe(LocalUsageClock { day_key: 20260827, hour: 23 }, [counter("a", 10)]));
        assert!(ledger.observe(LocalUsageClock { day_key: 20260827, hour: 23 }, [counter("a", 15)]));
        assert!(ledger.observe(LocalUsageClock { day_key: 20260828, hour: 0 }, [counter("a", 18)]));
        assert_eq!(ledger.today_counts(20260827).and_then(|counts| counts.total), Some(5));
        assert_eq!(ledger.today_counts(20260828).and_then(|counts| counts.total), Some(3));
    }

    #[test]
    fn recent_days_are_chronological_and_contain_only_aggregate_buckets() {
        let mut ledger = LocalUsageLedger::default();
        let day_one = LocalUsageClock { day_key: 20260827, hour: 8 };
        let day_two = LocalUsageClock { day_key: 20260828, hour: 9 };
        assert!(!ledger.observe(day_one, [counter("private-thread", 10)]));
        assert!(ledger.observe(day_one, [counter("private-thread", 22)]));
        assert!(ledger.observe(day_two, [counter("private-thread", 27)]));

        let recent = ledger.recent_days(14);
        assert_eq!(recent.iter().map(|day| day.day_key).collect::<Vec<_>>(), vec![20260827, 20260828]);
        assert_eq!(recent.iter().map(|day| day.total_tokens).collect::<Vec<_>>(), vec![12, 5]);
        let serialized = serde_json::to_string(&recent).expect("聚合桶可序列化");
        assert!(!serialized.contains("private-thread"));
    }

    #[test]
    fn session_day_rebuild_keeps_detailed_counts_and_is_idempotent() {
        let mut ledger = LocalUsageLedger::default();
        let first = TokenCounts {
            input: Some(100),
            cached_input: Some(60),
            output: Some(25),
            total: Some(125),
            ..TokenCounts::default()
        };
        let second = TokenCounts {
            input: Some(80),
            cached_input: Some(20),
            output: Some(30),
            total: Some(110),
            ..TokenCounts::default()
        };
        assert!(ledger.replace_session_day(20260831, [(8, first.clone()), (9, second.clone())]));
        assert!(!ledger.replace_session_day(20260831, [(8, first), (9, second)]));
        let counts = ledger.today_counts(20260831).expect("应有今日明细");
        assert_eq!(counts.input, Some(180));
        assert_eq!(counts.cached_input, Some(80));
        assert_eq!(counts.output, Some(55));
        assert_eq!(counts.total, Some(235));
        assert_eq!(ledger.today_hourly_tokens(20260831)[8], 125);
        assert_eq!(ledger.today_hourly_tokens(20260831)[9], 110);

        let restored = LocalUsageLedger::from_persisted(ledger.persisted());
        assert_eq!(restored.today_counts(20260831), Some(counts));
    }

    #[test]
    fn legacy_double_counted_cache_is_repaired_before_sqlite_roundtrip() {
        let mut hourly_total = [0; 24];
        let mut hourly_input = [0; 24];
        let mut hourly_cached = [0; 24];
        let mut hourly_output = [0; 24];
        hourly_total[9] = 205;
        hourly_input[9] = 100;
        hourly_cached[9] = 80;
        hourly_output[9] = 25;
        let persisted = PersistedLocalUsageLedger {
            version: LocalUsageLedger::VERSION,
            days: vec![PersistedUsageDay {
                day_key: 20260831,
                total_tokens: 205,
                hourly_tokens: hourly_total,
                input_tokens: 100,
                cached_input_tokens: 80,
                output_tokens: 25,
                hourly_input_tokens: hourly_input,
                hourly_cached_input_tokens: hourly_cached,
                hourly_output_tokens: hourly_output,
                estimated_api_cost_micro_usd: 0,
                hourly_estimated_api_cost_micro_usd: [0; 24],
            }],
        };
        let mut ledger = LocalUsageLedger::from_persisted(persisted);
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("codex-taskbar-ledger-{nonce}.db"));

        ledger.save_sqlite(&path).unwrap();
        let restored = LocalUsageLedger::load_sqlite(&path).unwrap();
        let counts = restored.today_counts(20260831).unwrap();
        assert_eq!(counts.total, Some(125));
        assert_eq!(counts.input, Some(100));
        assert_eq!(counts.cached_input, Some(80));
        assert_eq!(counts.output, Some(25));
        assert_eq!(restored.today_hourly_tokens(20260831)[9], 125);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn priced_session_events_persist_only_aggregate_micro_usd_buckets() {
        let mut ledger = LocalUsageLedger::default();
        let counts = TokenCounts {
            input: Some(1_200),
            cached_input: Some(800),
            output: Some(300),
            total: Some(1_500),
            ..TokenCounts::default()
        };
        assert!(ledger.replace_session_day_priced(20260831, [(8, counts.clone(), Some(125)), (9, counts, Some(375))],));
        assert_eq!(ledger.today_estimated_api_cost_micro_usd(20260831), Some(500));
        let hourly = ledger.today_hourly_estimated_api_cost_micro_usd(20260831);
        assert_eq!(hourly[8], 125);
        assert_eq!(hourly[9], 375);

        let serialized = serde_json::to_string(&ledger.persisted()).expect("应序列化聚合价格桶");
        assert!(!serialized.contains("gpt-"));
        let restored = LocalUsageLedger::from_persisted(serde_json::from_str(&serialized).expect("应反序列化"));
        assert_eq!(restored.today_estimated_api_cost_micro_usd(20260831), Some(500));
    }

    #[test]
    fn clear_removes_days_and_baselines_before_accepting_new_usage() {
        let mut ledger = LocalUsageLedger::default();
        let clock = LocalUsageClock { day_key: 20260831, hour: 12 };
        assert!(!ledger.observe(clock, [counter("private-thread", 100)]));
        assert!(ledger.observe(clock, [counter("private-thread", 140)]));
        ledger.clear();
        assert!(ledger.is_dirty());
        assert!(ledger.persisted().days.is_empty());
        assert!(ledger.today_counts(clock.day_key).is_none());

        // 清理同时丢弃基线；第一帧只重建基线，不能把清理前累计误算为新增。
        assert!(!ledger.observe(clock, [counter("private-thread", 150)]));
        assert!(ledger.observe(clock, [counter("private-thread", 155)]));
        assert_eq!(ledger.today_counts(clock.day_key).and_then(|counts| counts.total), Some(5));
    }
}

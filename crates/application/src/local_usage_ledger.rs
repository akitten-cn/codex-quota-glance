//! 不读取会话内容的本机 Token 增量账本。
//!
//! Codex 的 `state_*.sqlite` 只提供每线程累计 `tokens_used`。本模块只将后续
//! 正向差值归入本地日期/小时桶；首帧、新线程和累计回退只重建基线。持久化格式
//! 故意不包含线程标识、路径、标题、提示词或任何 `item_json` 内容。

use std::collections::BTreeMap;

use codex_taskbar_domain::usage::TokenCounts;
use rusqlite::{Connection, OptionalExtension, params};
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

/// 只包含数值、模型与事件时间的 session 投影。
#[derive(Debug, Clone)]
pub struct SessionUsageEvent {
    pub hour: u8,
    pub counts: TokenCounts,
    pub cost: Option<u64>,
    pub model: Option<String>,
    pub historical: bool,
    pub occurred_at_unix_ms: Option<i64>,
}

/// 在内存中维护线程基线，在磁盘上只维护桶总计。
#[derive(Debug, Clone)]
pub struct LocalUsageLedger {
    models: BTreeMap<(i32, String), ModelUsage>,
    baselines: BTreeMap<String, u64>,
    reconciliations: BTreeMap<i32, SessionReconciliation>,
    baseline_only_next: bool,
    days: BTreeMap<i32, UsageDay>,
    dirty: bool,
    retained_days: usize,
    cleared_at_ms: i64,
    migration_complete: bool,
}

impl Default for LocalUsageLedger {
    fn default() -> Self {
        Self {
            models: BTreeMap::new(),
            baselines: BTreeMap::new(),
            reconciliations: BTreeMap::new(),
            baseline_only_next: false,
            days: BTreeMap::new(),
            dirty: false,
            retained_days: 90,
            cleared_at_ms: 0,
            migration_complete: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub total_tokens: u64,
    pub cost_micro_usd: u64,
    pub unpriced_tokens: u64,
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

#[derive(Debug, Clone)]
struct SessionReconciliation {
    preserved: UsageDay,
    preserved_models: BTreeMap<String, ModelUsage>,
    recovered: UsageDay,
    recovered_models: BTreeMap<String, ModelUsage>,
    sqlite_pending: UsageDay,
    sqlite_cutoff_ms: Option<i64>,
    covered_live: UsageDay,
    covered_live_models: BTreeMap<String, ModelUsage>,
    session_active: bool,
    live: UsageDay,
    live_models: BTreeMap<String, ModelUsage>,
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
        // 部分明细不能冒充整日明细；保留未分解总量，明细完整后才向 UI 提供分项。
        if self.total_tokens != self.input_tokens.saturating_add(self.output_tokens) {
            return TokenCounts { total: (self.total_tokens > 0).then_some(self.total_tokens), ..Default::default() };
        }
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
    pub const VERSION: u8 = 4;
    const MAX_RETAINED_DAYS: usize = 365;

    /// 清空仅属于本应用的聚合历史，同时丢弃内存基线，避免清理后把旧累计误算为新增。
    /// 调用方应在用户明确二次确认后执行，并立即持久化空账本。
    pub fn clear(&mut self) {
        self.cleared_at_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        self.migration_complete = true;
        self.models.clear();
        self.baselines.clear();
        self.reconciliations.clear();
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
                        if persisted.version < 4
                            && (day.hourly_input_tokens[hour] > 0 || day.hourly_output_tokens[hour] > 0)
                        {
                            *hourly_total =
                                day.hourly_input_tokens[hour].saturating_add(day.hourly_output_tokens[hour]);
                        }
                    }
                    let detailed_total = day.input_tokens.saturating_add(day.output_tokens);
                    UsageDay {
                        total_tokens: if persisted.version < 4 && (day.input_tokens > 0 || day.output_tokens > 0) {
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
        Self {
            models: BTreeMap::new(),
            baselines: BTreeMap::new(),
            reconciliations: BTreeMap::new(),
            baseline_only_next: false,
            days,
            dirty: false,
            retained_days: 90,
            cleared_at_ms: 0,
            migration_complete: false,
        }
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
        let totals_version: i64 = connection
            .query_row("SELECT value FROM usage_metadata WHERE key='totals_version'", [], |row| row.get(0))
            .optional()?
            .unwrap_or(3);
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
                let canonical_total =
                    if totals_version < 4 && (input > 0 || output > 0) { input.saturating_add(output) } else { total };
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
                let canonical_total =
                    if totals_version < 4 && (input > 0 || output > 0) { input.saturating_add(output) } else { total };
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
        let mut models = BTreeMap::new();
        let mut statement = connection
            .prepare("SELECT day_key,model,total_tokens,cost_micro_usd,unpriced_tokens FROM usage_model_daily")?;
        for row in statement.query_map([], |row| {
            Ok((
                (row.get(0)?, row.get(1)?),
                ModelUsage { total_tokens: row.get(2)?, cost_micro_usd: row.get(3)?, unpriced_tokens: row.get(4)? },
            ))
        })? {
            let (key, value) = row?;
            models.insert(key, value);
        }
        let cleared_at_ms = connection
            .query_row("SELECT value FROM usage_metadata WHERE key='cleared_at_ms'", [], |row| row.get(0))
            .optional()?
            .unwrap_or(0);
        let migration_complete = connection
            .query_row("SELECT value FROM usage_metadata WHERE key='migration_complete'", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .unwrap_or(0)
            != 0;
        Ok(Self {
            models,
            baselines: BTreeMap::new(),
            reconciliations: BTreeMap::new(),
            baseline_only_next: false,
            days,
            dirty: repaired,
            retained_days: 90,
            cleared_at_ms,
            migration_complete,
        })
    }

    /// 用一个事务替换关系型聚合桶。最多 365×24 行且仅按低频批次调用，避免
    /// 每个 Token 或动画帧触发磁盘 I/O。
    pub fn save_sqlite(&mut self, path: &std::path::Path) -> rusqlite::Result<()> {
        let mut connection = open_usage_database(path)?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM usage_model_daily", [])?;
        for ((day, model), usage) in &self.models {
            transaction.execute(
                "INSERT INTO usage_model_daily VALUES(?1,?2,?3,?4,?5)",
                params![day, model, usage.total_tokens, usage.cost_micro_usd, usage.unpriced_tokens],
            )?;
        }
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
        transaction.execute(
            "INSERT OR REPLACE INTO usage_metadata(key,value) VALUES('cleared_at_ms',?1)",
            [self.cleared_at_ms],
        )?;
        transaction.execute("INSERT OR REPLACE INTO usage_metadata(key,value) VALUES('migration_complete',1)", [])?;
        transaction.execute("INSERT OR REPLACE INTO usage_metadata(key,value) VALUES('totals_version',4)", [])?;
        transaction.commit()?;
        self.migration_complete = true;
        self.mark_persisted();
        Ok(())
    }

    /// session 权威期间继续刷新 SQLite 基线，但不向日桶计费。
    pub fn refresh_baselines(&mut self, counters: impl IntoIterator<Item = ThreadTokenCounter>) {
        self.baseline_only_next = true;
        self.baselines = counters.into_iter().map(|counter| (counter.thread_id, counter.tokens_used)).collect();
    }

    /// 聚合原账本、恢复范围及实时范围，恢复补齐不能当作实时新增。
    pub fn reconcile_session_batch(
        &mut self,
        day: i32,
        bootstrap: bool,
        events: impl IntoIterator<Item = (u8, TokenCounts, Option<u64>, Option<String>, bool)>,
    ) {
        self.reconcile_session_batch_at(
            day,
            bootstrap,
            events.into_iter().map(|(hour, counts, cost, model, historical)| SessionUsageEvent {
                hour,
                counts,
                cost,
                model,
                historical,
                occurred_at_unix_ms: None,
            }),
        );
    }

    pub fn reconcile_session_batch_at(
        &mut self,
        day: i32,
        reset: bool,
        events: impl IntoIterator<Item = SessionUsageEvent>,
    ) {
        let events: Vec<_> = events.into_iter().collect();
        if !events.is_empty() {
            self.baseline_only_next = true;
        }
        if reset {
            let known_model_total = self
                .models
                .iter()
                .filter(|((key, _), _)| *key == day)
                .fold(0_u64, |sum, (_, usage)| sum.saturating_add(usage.total_tokens));
            let unattributed =
                self.days.get(&day).map_or(0, |bucket| bucket.total_tokens).saturating_sub(known_model_total);
            if unattributed > 0 {
                let usage = self.models.entry((day, "未识别模型".to_owned())).or_default();
                merge_model(
                    usage,
                    &ModelUsage { total_tokens: unattributed, unpriced_tokens: unattributed, cost_micro_usd: 0 },
                );
            }
            // 来源切换使先前累计基线失效；下一帧 SQLite 只能重新建基线。
            if !events.is_empty() {
                self.baselines.clear();
            }
            self.reconciliations.insert(
                day,
                SessionReconciliation {
                    preserved: self.days.get(&day).cloned().unwrap_or_else(UsageDay::empty),
                    preserved_models: self
                        .models
                        .iter()
                        .filter(|((key, _), _)| *key == day)
                        .map(|((_, model), usage)| (model.clone(), usage.clone()))
                        .collect(),
                    recovered: UsageDay::empty(),
                    recovered_models: BTreeMap::new(),
                    sqlite_pending: UsageDay::empty(),
                    sqlite_cutoff_ms: None,
                    covered_live: UsageDay::empty(),
                    covered_live_models: BTreeMap::new(),
                    session_active: false,
                    live: UsageDay::empty(),
                    live_models: BTreeMap::new(),
                },
            );
        }
        if let Some(state) = self.reconciliations.get_mut(&day) {
            state.session_active |= !events.is_empty();
            for SessionUsageEvent { hour, counts, cost, model, historical, occurred_at_unix_ms } in events {
                let (bucket, models) = if historical {
                    (&mut state.recovered, &mut state.recovered_models)
                } else if occurred_at_unix_ms.zip(state.sqlite_cutoff_ms).is_some_and(|(event, cutoff)| event <= cutoff)
                {
                    (&mut state.covered_live, &mut state.covered_live_models)
                } else {
                    (&mut state.live, &mut state.live_models)
                };
                bucket.add_counts(usize::from(hour.min(23)), &counts, cost);
                add_model(models, model.as_deref(), &counts, cost);
            }
            // 两个来源都覆盖截至 SQLite 采样边界的累计范围：
            // 已存基线 + SQLite 差额，与 session 历史恢复 + 该区间 live。
            // 历史分类不代表与 SQLite 不重叠，禁止先按历史分组求和。
            let mut sqlite_coverage = state.preserved.clone();
            merge_day(&mut sqlite_coverage, &state.sqlite_pending);
            // 已存基线属于扫描前，明确的 covered_live 属于扫描后；两者可相加。
            // 即使 session 历史尚未补齐，也不能把已确认的 live 压在旧总量下面。
            let (mut session_coverage, mut session_models) =
                if state.recovered.total_tokens >= state.preserved.total_tokens {
                    (state.recovered.clone(), state.recovered_models.clone())
                } else {
                    (state.preserved.clone(), state.preserved_models.clone())
                };
            merge_day(&mut session_coverage, &state.covered_live);
            for (name, usage) in &state.covered_live_models {
                merge_model(session_models.entry(name.clone()).or_default(), usage);
            }
            let (mut bucket, mut models) = if session_coverage.total_tokens >= sqlite_coverage.total_tokens {
                (session_coverage, session_models)
            } else {
                let mut models = state.preserved_models.clone();
                let total = state.sqlite_pending.total_tokens;
                if total > 0 {
                    merge_model(
                        models.entry("未识别模型".to_owned()).or_default(),
                        &ModelUsage { total_tokens: total, cost_micro_usd: 0, unpriced_tokens: total },
                    );
                }
                (sqlite_coverage, models)
            };
            // 只有采样边界后的 live 才属于明确不重叠的新增范围。
            merge_day(&mut bucket, &state.live);
            for (name, usage) in &state.live_models {
                merge_model(models.entry(name.clone()).or_default(), usage);
            }
            self.days.insert(day, bucket);
            self.models.retain(|(key, _), _| *key != day);
            self.models.extend(models.into_iter().map(|(model, usage)| ((day, model), usage)));
            self.dirty = true;
        } else {
            for SessionUsageEvent { hour, counts, cost, model, .. } in events {
                self.observe_session_event_priced(LocalUsageClock { day_key: day, hour }, &counts, cost);
                self.observe_model_event(day, model.as_deref(), &counts, cost);
            }
        }
        self.prune_days();
    }

    /// 清理写入成功后才提交内存状态；失败保留原账本及截止时间。
    pub fn clear_and_save(&mut self, path: &std::path::Path) -> rusqlite::Result<()> {
        let mut cleared = self.clone();
        cleared.clear();
        cleared.save_sqlite(path)?;
        *self = cleared;
        Ok(())
    }

    /// 接纳一帧线程累计量。返回值只代表本地统计是否新增，便于调用方节流落盘。
    pub fn observe(&mut self, clock: LocalUsageClock, counters: impl IntoIterator<Item = ThreadTokenCounter>) -> bool {
        self.observe_at(clock, counters, (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64)
    }

    pub fn observe_at(
        &mut self,
        clock: LocalUsageClock,
        counters: impl IntoIterator<Item = ThreadTokenCounter>,
        observed_at_unix_ms: i64,
    ) -> bool {
        if self.baseline_only_next {
            self.baselines.clear();
            self.baseline_only_next = false;
        }
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
        if let Some(state) = self.reconciliations.get_mut(&clock.day_key)
            && !state.session_active
        {
            state.sqlite_pending.add_counts(hour, &TokenCounts { total: Some(delta), ..Default::default() }, None);
            state.sqlite_cutoff_ms = Some(observed_at_unix_ms);
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

    /// Bootstrap replaces only this day's model buckets; repeated scans cannot double count.
    pub fn replace_model_day(&mut self, day: i32) {
        self.models.retain(|(key, _), _| *key != day);
        self.dirty = true;
    }

    pub fn observe_model_event(&mut self, day: i32, model: Option<&str>, counts: &TokenCounts, cost: Option<u64>) {
        let total = counts.display_total().unwrap_or(0);
        if total == 0 {
            return;
        }
        let model = model.map(str::trim).filter(|name| !name.is_empty()).unwrap_or("未识别模型");
        let usage = self.models.entry((day, model.to_owned())).or_default();
        usage.total_tokens = usage.total_tokens.saturating_add(total);
        if let Some(cost) = cost {
            usage.cost_micro_usd = usage.cost_micro_usd.saturating_add(cost);
        } else {
            usage.unpriced_tokens = usage.unpriced_tokens.saturating_add(total);
        }
        self.dirty = true;
    }

    pub fn today_models(&self, day: i32) -> Vec<(&str, &ModelUsage)> {
        let mut rows: Vec<_> = self
            .models
            .iter()
            .filter(|((key, _), usage)| *key == day && usage.total_tokens > 0)
            .map(|((_, model), usage)| (model.as_str(), usage))
            .collect();
        rows.sort_by(|a, b| b.1.total_tokens.cmp(&a.1.total_tokens).then(a.0.cmp(b.0)));
        rows
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

    pub fn cleared_at_ms(&self) -> i64 {
        self.cleared_at_ms
    }

    pub fn migration_complete(&self) -> bool {
        self.migration_complete
    }

    /// 平台每轮提供当前本地自然日，启动及跨日均立即裁剪。
    pub fn prune_at_day(&mut self, day_key: i32) {
        let Some(date) = calendar_date(day_key) else { return };
        let cutoff = date - time::Duration::days(self.retained_days.saturating_sub(1) as i64);
        let before = self.days.len();
        self.days.retain(|key, _| calendar_date(*key).is_some_and(|date| date >= cutoff));
        self.models.retain(|(day, _), _| self.days.contains_key(day));
        self.reconciliations.retain(|day, _| self.days.contains_key(day));
        self.dirty |= before != self.days.len();
    }

    fn prune_days(&mut self) {
        if let Some(day) = self.days.keys().next_back().copied() {
            self.prune_at_day(day);
        }
        while self.days.len() > self.retained_days {
            let Some(first) = self.days.first_key_value().map(|(key, _)| *key) else { break };
            self.days.remove(&first);
        }
        self.models.retain(|(day, _), _| self.days.contains_key(day));
        self.reconciliations.retain(|day, _| self.days.contains_key(day));
    }
}

fn merge_model(target: &mut ModelUsage, source: &ModelUsage) {
    target.total_tokens = target.total_tokens.saturating_add(source.total_tokens);
    target.cost_micro_usd = target.cost_micro_usd.saturating_add(source.cost_micro_usd);
    target.unpriced_tokens = target.unpriced_tokens.saturating_add(source.unpriced_tokens);
}

fn add_model(models: &mut BTreeMap<String, ModelUsage>, model: Option<&str>, counts: &TokenCounts, cost: Option<u64>) {
    let total = counts.display_total().unwrap_or(0);
    let usage = models.entry(model.unwrap_or("未识别模型").to_owned()).or_default();
    merge_model(
        usage,
        &ModelUsage {
            total_tokens: total,
            cost_micro_usd: cost.unwrap_or(0),
            unpriced_tokens: if cost.is_none() { total } else { 0 },
        },
    );
}

fn merge_day(target: &mut UsageDay, source: &UsageDay) {
    macro_rules! scalar { ($($field:ident),*) => { $(target.$field = target.$field.saturating_add(source.$field);)* }; }
    scalar!(total_tokens, input_tokens, cached_input_tokens, output_tokens, estimated_api_cost_micro_usd);
    for hour in 0..24 {
        macro_rules! hourly { ($($field:ident),*) => { $(target.$field[hour] = target.$field[hour].saturating_add(source.$field[hour]);)* }; }
        hourly!(
            hourly_tokens,
            hourly_input_tokens,
            hourly_cached_input_tokens,
            hourly_output_tokens,
            hourly_estimated_api_cost_micro_usd
        );
    }
}

fn calendar_date(key: i32) -> Option<time::Date> {
    time::Date::from_calendar_date(
        key / 10000,
        time::Month::try_from(((key / 100) % 100) as u8).ok()?,
        (key % 100) as u8,
    )
    .ok()
}

fn open_usage_database(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS usage_metadata(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS usage_model_daily(
           day_key INTEGER NOT NULL, model TEXT NOT NULL,
           total_tokens INTEGER NOT NULL, cost_micro_usd INTEGER NOT NULL,
           unpriced_tokens INTEGER NOT NULL, PRIMARY KEY(day_key,model)
         );
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

    #[test]
    fn model_daily_usage_is_sparse_idempotent_and_persistent() {
        let mut ledger = LocalUsageLedger::default();
        let counts =
            TokenCounts { input: Some(100), cached_input: Some(80), output: Some(20), ..TokenCounts::default() };
        let day = 20260905;
        ledger.observe_session_event(LocalUsageClock { day_key: day, hour: 1 }, &counts);
        for _ in 0..2 {
            ledger.replace_model_day(day);
            ledger.observe_model_event(day, Some("gpt-6-astra"), &counts, Some(100));
            ledger.observe_model_event(day, Some("unpriced"), &counts, None);
        }
        let rows = ledger.today_models(day);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.total_tokens, 120); // cached input is already included
        assert_eq!(rows[0].1.cost_micro_usd, 100);
        assert_eq!(rows[1].1.unpriced_tokens, 120);
        assert!(ledger.today_models(day + 1).is_empty());
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("codex-model-ledger-{nonce}.db"));
        ledger.save_sqlite(&path).unwrap();
        let mut restored = LocalUsageLedger::load_sqlite(&path).unwrap();
        assert_eq!(ledger.today_models(day), restored.today_models(day));
        restored.clear();
        restored.save_sqlite(&path).unwrap();
        assert!(LocalUsageLedger::load_sqlite(&path).unwrap().today_models(day).is_empty());
        std::fs::remove_file(path).unwrap();
    }

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
            version: 3,
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
    fn trend_read_repairs_legacy_hourly_total_without_rewriting_database_first() {
        let mut hourly_total = [0; 24];
        let mut hourly_input = [0; 24];
        let mut hourly_cached = [0; 24];
        let mut hourly_output = [0; 24];
        hourly_total[11] = 90_900_000;
        hourly_input[11] = 45_500_000;
        hourly_cached[11] = 43_300_000;
        hourly_output[11] = 192_000;
        let ledger = LocalUsageLedger::from_persisted(PersistedLocalUsageLedger {
            version: 3,
            days: vec![PersistedUsageDay {
                day_key: 20260907,
                total_tokens: 90_900_000,
                hourly_tokens: hourly_total,
                input_tokens: 45_500_000,
                cached_input_tokens: 43_300_000,
                output_tokens: 192_000,
                hourly_input_tokens: hourly_input,
                hourly_cached_input_tokens: hourly_cached,
                hourly_output_tokens: hourly_output,
                estimated_api_cost_micro_usd: 0,
                hourly_estimated_api_cost_micro_usd: [0; 24],
            }],
        });
        assert_eq!(ledger.today_hourly_tokens(20260907)[11], 45_692_000);
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
    #[test]
    fn calendar_retention_handles_leap_month_and_inactive_days() {
        let mut ledger = LocalUsageLedger::default();
        for day in [20240228, 20240229, 20240301] {
            ledger.replace_session_day(day, [(12, TokenCounts { total: Some(10), ..Default::default() })]);
        }
        ledger.set_retention_days(2);
        assert_eq!(ledger.recent_days(365).iter().map(|day| day.day_key).collect::<Vec<_>>(), vec![20240229, 20240301]);
        ledger.prune_at_day(20240303);
        assert!(ledger.recent_days(365).is_empty());
        assert!(ledger.is_dirty());
    }

    #[test]
    fn locked_database_read_fails_and_original_history_is_readable_after_unlock() {
        let path = std::env::temp_dir().join(format!(
            "taskbar-locked-db-{}-{}.sqlite",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let mut ledger = LocalUsageLedger::default();
        ledger.replace_session_day(20260922, [(12, TokenCounts { total: Some(123), ..Default::default() })]);
        ledger.save_sqlite(&path).unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; UPDATE usage_daily SET total_tokens=123;")
            .unwrap();
        assert!(LocalUsageLedger::load_sqlite(&path).is_err());
        drop(lock);
        assert_eq!(LocalUsageLedger::load_sqlite(&path).unwrap().today_counts(20260922).unwrap().total, Some(123));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn failed_clear_preserves_memory_cutoff_and_database() {
        let path = std::env::temp_dir()
            .join(format!("taskbar-clear-failure-{}.sqlite", time::OffsetDateTime::now_utc().unix_timestamp_nanos()));
        let mut ledger = LocalUsageLedger::default();
        ledger.replace_session_day(20260922, [(12, TokenCounts { total: Some(300), ..Default::default() })]);
        ledger.save_sqlite(&path).unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(ledger.clear_and_save(&path).is_err());
        assert_eq!(ledger.cleared_at_ms(), 0);
        assert_eq!(ledger.today_counts(20260922).unwrap().display_total(), Some(300));
        drop(lock);
        assert_eq!(
            LocalUsageLedger::load_sqlite(&path).unwrap().today_counts(20260922).unwrap().display_total(),
            Some(300)
        );
        ledger.clear_and_save(&path).unwrap();
        assert!(LocalUsageLedger::load_sqlite(&path).unwrap().today_counts(20260922).is_none());
        std::fs::remove_file(path).unwrap();
    }
}

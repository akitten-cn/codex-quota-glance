//! Codex SQLite 的保守只读后备适配器。
//!
//! SQLite 并非 Codex 的稳定公开接口。本 crate 先检查表和列能力，只接受本模块明确
//! 声明的候选 schema；未知结构返回 `UnsupportedSchema`，绝不按名称猜测数据含义。
//! 已观察到的真实 schema 仍必须在目标 Codex 版本上进行实机只读验证后才可扩大支持范围。

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use codex_taskbar_domain::activity::{ActivityState, aggregate};
use rusqlite::{Connection, Error as SqlError, OpenFlags, OptionalExtension, ffi::ErrorCode};

/// 是否显式允许 SQLite 的 `immutable=1` URI 参数。
///
/// 该参数可能忽略正在进行的 WAL/锁状态，只适用于调用方确认数据库为不会变化的副本。
/// 默认关闭，以免把活跃 Codex 数据库的旧快照误当成实时数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImmutableMode {
    #[default]
    Disabled,
    Explicit,
}

/// SQLite 只读连接配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteFallbackConfig {
    pub database_path: PathBuf,
    pub busy_timeout: Duration,
    /// 最近一次安全 item 心跳的短租约；过期后仍会受 `activity_hard_ttl` 保护。
    pub activity_freshness_ttl: Duration,
    /// `thread_history` 中没有终态的 `inProgress` Turn 最长可作为低置信活动保留多久。
    ///
    /// 长推理或自定义交互组件阶段可能数分钟不写入 item；不能在短租约到期时立刻
    /// 误报“未知”。硬上限同时防止异常退出遗留的 `inProgress` 在任务结束后仍长期
    /// 保持紫色运行态。到期后按空闲处理，而非把低置信旧记录继续显示为活动。
    pub activity_hard_ttl: Duration,
    pub immutable: ImmutableMode,
}

impl SqliteFallbackConfig {
    /// 构造保守默认值：只读 URI、250ms 锁等待、不启用 immutable。
    #[must_use]
    pub fn new(database_path: impl Into<PathBuf>) -> Self {
        Self {
            database_path: database_path.into(),
            busy_timeout: Duration::from_millis(250),
            activity_freshness_ttl: Duration::from_secs(30),
            activity_hard_ttl: Duration::from_secs(12 * 60),
            immutable: ImmutableMode::Disabled,
        }
    }

    /// 返回可安全记录的路径摘要，永不返回完整用户目录。
    #[must_use]
    pub fn redacted_location(&self) -> RedactedLocation {
        RedactedLocation::from_path(&self.database_path)
    }
}

/// 用于日志的数据库位置摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedLocation {
    pub file_name: String,
}

impl RedactedLocation {
    fn from_path(path: &Path) -> Self {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("<database>")
            .to_owned();
        Self { file_name }
    }
}

impl fmt::Display for RedactedLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "…/{}", self.file_name)
    }
}

/// 只读后备适配器。它没有任何建表、迁移或写入 API。
#[derive(Debug, Clone)]
pub struct CodexSqliteFallback {
    config: SqliteFallbackConfig,
}

impl CodexSqliteFallback {
    #[must_use]
    pub fn new(config: SqliteFallbackConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn config(&self) -> &SqliteFallbackConfig {
        &self.config
    }

    /// 打开 `mode=ro` URI，检查 schema，并读取一份明确标记为后备的数据快照。
    ///
    /// 此方法将所有失败编码在 `health` 中，方便上层无日志泄露地降级；快照为 `None`
    /// 时不得用它覆盖来自 App Server 的权威数据。
    #[must_use]
    pub fn read_snapshot(&self) -> FallbackReadReport {
        let connection = match self.open_readonly() {
            Ok(connection) => connection,
            Err(error) => return FallbackReadReport::unavailable(classify_error(&error)),
        };
        let inspection = match inspect_schema(&connection) {
            Ok(inspection) => inspection,
            Err(error) => return FallbackReadReport::unavailable(classify_error(&error)),
        };
        if let Some(schema) = StateThreadsSchema::detect(&inspection) {
            return match read_state_threads_snapshot(&connection, &schema) {
                Ok(snapshot) => FallbackReadReport {
                    health: FallbackHealth::Available { schema: schema.summary() },
                    snapshot: Some(snapshot),
                },
                Err(error) => FallbackReadReport::unavailable(classify_error(&error)),
            };
        }
        if let Some(schema) = ThreadHistoryActivitySchema::detect(&inspection) {
            return match read_thread_history_activity(
                &connection,
                &schema,
                self.config.activity_freshness_ttl,
                self.config.activity_hard_ttl,
            ) {
                Ok(snapshot) => FallbackReadReport {
                    health: FallbackHealth::Available { schema: schema.summary() },
                    snapshot: Some(snapshot),
                },
                Err(error) => FallbackReadReport::unavailable(classify_error(&error)),
            };
        }
        let schema = match SupportedSchema::detect(&inspection) {
            Ok(schema) => schema,
            Err(missing_capabilities) => {
                return FallbackReadReport {
                    health: FallbackHealth::UnsupportedSchema { inspection, missing_capabilities },
                    snapshot: None,
                };
            }
        };
        match read_supported_snapshot(&connection, &schema) {
            Ok(snapshot) => FallbackReadReport {
                health: FallbackHealth::Available { schema: schema.summary() },
                snapshot: Some(snapshot),
            },
            Err(error) => FallbackReadReport::unavailable(classify_error(&error)),
        }
    }

    /// 读取未归档线程的结构化累计 Token。此接口只服务于本机增量账本：调用方必须
    /// 把首帧当作基线，不能把累计值直接宣称为“今日消耗”。线程标识仅允许在进程
    /// 内进行匹配，不得写入日志或持久化文件。
    #[must_use]
    pub fn read_thread_token_totals(&self) -> ThreadTokenTotalsReport {
        let connection = match self.open_readonly() {
            Ok(connection) => connection,
            Err(error) => return ThreadTokenTotalsReport::unavailable(classify_error(&error)),
        };
        let inspection = match inspect_schema(&connection) {
            Ok(inspection) => inspection,
            Err(error) => return ThreadTokenTotalsReport::unavailable(classify_error(&error)),
        };
        let Some(schema) = StateThreadsSchema::detect(&inspection) else {
            return ThreadTokenTotalsReport {
                health: FallbackHealth::UnsupportedSchema {
                    inspection,
                    missing_capabilities: vec!["threads.id / updated_at_ms / tokens_used".to_owned()],
                },
                totals: None,
            };
        };
        match read_state_thread_token_totals(&connection, &schema) {
            Ok(totals) => ThreadTokenTotalsReport {
                health: FallbackHealth::Available { schema: schema.summary() },
                totals: Some(totals),
            },
            Err(error) => ThreadTokenTotalsReport::unavailable(classify_error(&error)),
        }
    }

    fn open_readonly(&self) -> Result<Connection, SqlError> {
        let uri = readonly_uri(&self.config.database_path, self.config.immutable);
        let connection =
            Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
        connection.busy_timeout(self.config.busy_timeout)?;
        Ok(connection)
    }
}

/// 一次后备读取的值与健康状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackReadReport {
    pub health: FallbackHealth,
    pub snapshot: Option<FallbackSnapshot>,
}

/// 一次结构化线程累计读取。`totals=None` 时上层只保留现有账本，不能将其归零。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTokenTotalsReport {
    pub health: FallbackHealth,
    pub totals: Option<Vec<ThreadTokenTotal>>,
}

impl ThreadTokenTotalsReport {
    fn unavailable(failure: FallbackFailure) -> Self {
        Self { health: FallbackHealth::Unavailable { failure }, totals: None }
    }
}

/// 仅在内存中使用的线程累计量。它不含标题、路径、消息或 item JSON。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTokenTotal {
    pub thread_id: String,
    pub tokens_used: u64,
    pub updated_at_unix_ms: i64,
}

impl FallbackReadReport {
    fn unavailable(failure: FallbackFailure) -> Self {
        Self { health: FallbackHealth::Unavailable { failure }, snapshot: None }
    }
}

/// SQLite 后备数据源的可用性。错误信息中不包含路径、SQL 或数据库内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackHealth {
    Available { schema: SupportedSchemaSummary },
    UnsupportedSchema { inspection: SchemaInspection, missing_capabilities: Vec<String> },
    Unavailable { failure: FallbackFailure },
}

/// 不可用的、可安全记录的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackFailure {
    Busy,
    Corrupt,
    Unreadable,
}

/// 已被本模块明确识别的 schema 能力摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportedSchemaSummary {
    pub thread_table: String,
    pub turn_table: Option<String>,
    pub item_table: Option<String>,
    pub token_table: Option<String>,
}

/// 在 SQLite 中观察到的公开结构；仅包含表名和列名，不读取业务内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInspection {
    pub tables: Vec<TableInspection>,
}

/// 一个表的名称与列名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInspection {
    pub name: String,
    pub columns: Vec<String>,
}

/// SQLite 降级快照。所有计数来自同一只读连接，且不声称代表 App Server 的额度数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackSnapshot {
    pub latest_thread_id: Option<String>,
    pub latest_turn_id: Option<String>,
    pub latest_item_id: Option<String>,
    pub activity: ActivityState,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// `state_5.threads.tokens_used` 的原始累计值；不推断输入、输出或今日用量。
    pub raw_thread_tokens_used: Option<u64>,
    /// 仅在内存中交给本机增量账本的结构化线程累计；不可持久化、不可记录日志。
    pub thread_token_totals: Vec<ThreadTokenTotal>,
    /// 此来源永远是非权威后备数据，上层应保留 stale 标识。
    pub is_stale_fallback: bool,
    /// `thread_history` 的活动状态只属于低置信探测，并受 freshness TTL 限制。
    pub activity_is_low_confidence: bool,
    pub activity_stale_after_unix_ms: Option<i64>,
    pub observed_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
struct TableLayout {
    name: String,
    columns: BTreeMap<String, String>,
}

/// 已实机确认 `state_5.sqlite` 的最小只读能力。
///
/// `tokens_used` 仅作为原始线程累计值返回；其语义不足以安全推断输入、输出或今日用量。
#[derive(Debug, Clone)]
struct StateThreadsSchema {
    table: TableLayout,
    id: String,
    updated_at_ms: String,
    tokens_used: String,
    archived: Option<String>,
}

impl StateThreadsSchema {
    fn detect(inspection: &SchemaInspection) -> Option<Self> {
        let table = inspection.tables.iter().find(|table| table.name.eq_ignore_ascii_case("threads"))?;
        let table = TableLayout {
            name: table.name.clone(),
            columns: table.columns.iter().map(|column| (column.to_ascii_lowercase(), column.clone())).collect(),
        };
        Some(Self {
            id: optional_column(&table, &["id"])?,
            updated_at_ms: optional_column(&table, &["updated_at_ms"])?,
            tokens_used: optional_column(&table, &["tokens_used"])?,
            archived: optional_column(&table, &["archived"]),
            table,
        })
    }

    fn summary(&self) -> SupportedSchemaSummary {
        SupportedSchemaSummary {
            thread_table: self.table.name.clone(),
            turn_table: None,
            item_table: None,
            token_table: None,
        }
    }
}

/// 已实机观察到 `thread_history_*.sqlite` 的活动探测能力。
///
/// 只读取 `thread_turns` 的状态和时间列，并可选读取同一 Turn 的
/// `thread_items.created_at_ms` 作为安全的活动心跳；即使数据库存在
/// `thread_items.item_json`，本适配器也不读取、不记录，更不会把 JSON 内容暴露给调用方。
#[derive(Debug, Clone)]
struct ThreadHistoryActivitySchema {
    table: TableLayout,
    status: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    item: Option<ThreadHistoryItemHeartbeatSchema>,
}

/// `thread_items` 中可公开且不含内容正文的活动心跳列。
#[derive(Debug, Clone)]
struct ThreadHistoryItemHeartbeatSchema {
    table: TableLayout,
    thread_id: String,
    turn_id: String,
    created_at_ms: String,
    /// 仅使用 Codex 写入的项目类别，不读取 `item_json` 内容。
    item_type: Option<String>,
}

/// 一个进行中 Turn 的安全活动证据，不含线程标识和任何项目正文。
#[derive(Debug)]
struct ThreadHistoryActivityRecord {
    status: String,
    started_at_ms: i64,
    heartbeat_at_ms: Option<i64>,
    latest_item_type: Option<String>,
}

impl ThreadHistoryActivitySchema {
    fn detect(inspection: &SchemaInspection) -> Option<Self> {
        let table = inspection.tables.iter().find(|table| table.name.eq_ignore_ascii_case("thread_turns"))?;
        let table = TableLayout {
            name: table.name.clone(),
            columns: table.columns.iter().map(|column| (column.to_ascii_lowercase(), column.clone())).collect(),
        };
        let item =
            inspection.tables.iter().find(|item| item.name.eq_ignore_ascii_case("thread_items")).and_then(|item| {
                let item = TableLayout {
                    name: item.name.clone(),
                    columns: item.columns.iter().map(|column| (column.to_ascii_lowercase(), column.clone())).collect(),
                };
                Some(ThreadHistoryItemHeartbeatSchema {
                    thread_id: optional_column(&item, &["thread_id"])?,
                    turn_id: optional_column(&item, &["turn_id"])?,
                    created_at_ms: optional_column(&item, &["created_at_ms"])?,
                    item_type: optional_column(&item, &["item_type"]),
                    table: item,
                })
            });
        Some(Self {
            status: optional_column(&table, &["status"])?,
            thread_id: optional_column(&table, &["thread_id"]),
            turn_id: optional_column(&table, &["turn_id"]),
            started_at: optional_column(&table, &["started_at"])?,
            completed_at: optional_column(&table, &["completed_at"]),
            item,
            table,
        })
    }

    fn summary(&self) -> SupportedSchemaSummary {
        SupportedSchemaSummary {
            thread_table: self.table.name.clone(),
            turn_table: Some(self.table.name.clone()),
            item_table: None,
            token_table: None,
        }
    }
}

#[derive(Debug, Clone)]
struct SupportedSchema {
    thread: TableLayout,
    thread_id: String,
    thread_recent: String,
    turn: TableLayout,
    turn_id: String,
    turn_thread_id: String,
    turn_status: String,
    turn_recent: String,
    item: Option<(TableLayout, String, String, String, String)>,
    token: TableLayout,
    token_thread_id: String,
    token_input: Option<String>,
    token_output: Option<String>,
    token_cached: Option<String>,
    token_total: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct TokenTotals {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    total: Option<u64>,
}

impl SupportedSchema {
    fn detect(inspection: &SchemaInspection) -> Result<Self, Vec<String>> {
        let tables = inspection
            .tables
            .iter()
            .map(|table| {
                (
                    table.name.to_ascii_lowercase(),
                    TableLayout {
                        name: table.name.clone(),
                        columns: table
                            .columns
                            .iter()
                            .map(|column| (column.to_ascii_lowercase(), column.clone()))
                            .collect(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut missing = Vec::new();
        let thread = candidate_table(&tables, &["threads", "thread"]);
        let turn = candidate_table(&tables, &["turns", "turn"]);
        let token = candidate_table(&tables, &["token_usage", "token_usages", "thread_token_usage"]);
        if thread.is_none() {
            missing.push("线程表（threads/thread）".to_owned());
        }
        if turn.is_none() {
            missing.push("回合表（turns/turn）".to_owned());
        }
        if token.is_none() {
            missing.push("token 表（token_usage/token_usages/thread_token_usage）".to_owned());
        }
        let (Some(thread), Some(turn), Some(token)) = (thread, turn, token) else {
            return Err(missing);
        };

        let thread_id = required_column(&thread, &["id", "thread_id"], "线程标识", &mut missing);
        let thread_recent = required_column(
            &thread,
            &["updated_at", "updatedat", "created_at", "createdat", "timestamp"],
            "线程时间",
            &mut missing,
        );
        let turn_id = required_column(&turn, &["id", "turn_id"], "回合标识", &mut missing);
        let turn_thread_id = required_column(&turn, &["thread_id", "threadid"], "回合线程关联", &mut missing);
        let turn_status = required_column(&turn, &["status", "state"], "回合状态", &mut missing);
        let turn_recent = required_column(
            &turn,
            &["updated_at", "updatedat", "created_at", "createdat", "timestamp"],
            "回合时间",
            &mut missing,
        );
        let token_thread_id = required_column(&token, &["thread_id", "threadid"], "token 线程关联", &mut missing);
        let token_input = optional_column(&token, &["input_tokens", "inputtokens"]);
        let token_output = optional_column(&token, &["output_tokens", "outputtokens"]);
        let token_cached = optional_column(&token, &["cached_input_tokens", "cachedinputtokens", "cache_read_tokens"]);
        let token_total = optional_column(&token, &["total_tokens", "totaltokens"]);
        if [token_input.as_ref(), token_output.as_ref(), token_cached.as_ref(), token_total.as_ref()]
            .iter()
            .all(Option::is_none)
        {
            missing.push("至少一个 token 计数列".to_owned());
        }
        let Some((thread_id, thread_recent, turn_id, turn_thread_id, turn_status, turn_recent, token_thread_id)) =
            thread_id
                .zip(thread_recent)
                .zip(turn_id)
                .zip(turn_thread_id)
                .zip(turn_status)
                .zip(turn_recent)
                .zip(token_thread_id)
                .map(|((((((a, b), c), d), e), f), g)| (a, b, c, d, e, f, g))
        else {
            return Err(missing);
        };
        if !missing.is_empty() {
            return Err(missing);
        }

        let item = candidate_table(&tables, &["items", "item"]).and_then(|item| {
            Some((
                item.clone(),
                optional_column(&item, &["id", "item_id"])?,
                optional_column(&item, &["turn_id", "turnid"])?,
                optional_column(&item, &["status", "state"])?,
                optional_column(&item, &["updated_at", "updatedat", "created_at", "createdat", "timestamp"])?,
            ))
        });
        Ok(Self {
            thread,
            thread_id,
            thread_recent,
            turn,
            turn_id,
            turn_thread_id,
            turn_status,
            turn_recent,
            item,
            token,
            token_thread_id,
            token_input,
            token_output,
            token_cached,
            token_total,
        })
    }

    fn summary(&self) -> SupportedSchemaSummary {
        SupportedSchemaSummary {
            thread_table: self.thread.name.clone(),
            turn_table: Some(self.turn.name.clone()),
            item_table: self.item.as_ref().map(|item| item.0.name.clone()),
            token_table: Some(self.token.name.clone()),
        }
    }
}

fn read_supported_snapshot(connection: &Connection, schema: &SupportedSchema) -> Result<FallbackSnapshot, SqlError> {
    let thread_sql = format!(
        "SELECT {} FROM {} ORDER BY {} DESC LIMIT 1",
        ident(&schema.thread_id),
        ident(&schema.thread.name),
        ident(&schema.thread_recent)
    );
    let latest_thread_id = connection.query_row(&thread_sql, [], |row| row.get::<_, String>(0)).optional()?;
    let Some(thread_id) = latest_thread_id.as_deref() else {
        return Ok(empty_snapshot());
    };
    let turn_sql = format!(
        "SELECT {}, {} FROM {} WHERE {} = ?1 ORDER BY {} DESC LIMIT 1",
        ident(&schema.turn_id),
        ident(&schema.turn_status),
        ident(&schema.turn.name),
        ident(&schema.turn_thread_id),
        ident(&schema.turn_recent)
    );
    let (latest_turn_id, turn_status) = connection
        .query_row(&turn_sql, [thread_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))
        .optional()?
        .unwrap_or((String::new(), None));
    let latest_turn_id = (!latest_turn_id.is_empty()).then_some(latest_turn_id);

    let (latest_item_id, item_status) = match (&schema.item, latest_turn_id.as_deref()) {
        (Some((item, item_id, item_turn_id, item_status, item_recent)), Some(turn_id)) => {
            let sql = format!(
                "SELECT {}, {} FROM {} WHERE {} = ?1 ORDER BY {} DESC LIMIT 1",
                ident(item_id),
                ident(item_status),
                ident(&item.name),
                ident(item_turn_id),
                ident(item_recent)
            );
            connection
                .query_row(&sql, [turn_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))
                .optional()?
                .unwrap_or((String::new(), None))
        }
        _ => (String::new(), None),
    };
    let latest_item_id = (!latest_item_id.is_empty()).then_some(latest_item_id);
    let tokens = read_token_totals(connection, schema, thread_id)?;
    let activity =
        aggregate([activity_from_status(turn_status.as_deref()), activity_from_status(item_status.as_deref())]);
    Ok(FallbackSnapshot {
        latest_thread_id,
        latest_turn_id,
        latest_item_id,
        activity,
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cached_input_tokens: tokens.cached,
        total_tokens: tokens.total,
        raw_thread_tokens_used: None,
        thread_token_totals: Vec::new(),
        is_stale_fallback: true,
        activity_is_low_confidence: false,
        activity_stale_after_unix_ms: None,
        observed_at_unix_ms: now_unix_ms(),
    })
}

fn read_state_threads_snapshot(
    connection: &Connection,
    schema: &StateThreadsSchema,
) -> Result<FallbackSnapshot, SqlError> {
    let filter = schema.archived.as_ref().map_or_else(
        || format!(" WHERE {} IS NOT NULL", ident(&schema.tokens_used)),
        |column| format!(" WHERE COALESCE({}, 0) = 0 AND {} IS NOT NULL", ident(column), ident(&schema.tokens_used)),
    );
    let sql = format!(
        "SELECT {}, {} FROM {}{} ORDER BY {} DESC LIMIT 1",
        ident(&schema.id),
        ident(&schema.tokens_used),
        ident(&schema.table.name),
        filter,
        ident(&schema.updated_at_ms)
    );
    let row = connection
        .query_row(&sql, [], |row| Ok((row.get::<_, String>(0)?, nonnegative(row.get::<_, Option<i64>>(1)?))))
        .optional()?;
    let mut snapshot = empty_snapshot();
    if let Some((id, tokens_used)) = row {
        snapshot.latest_thread_id = Some(id);
        snapshot.raw_thread_tokens_used = tokens_used;
    }
    snapshot.thread_token_totals = read_state_thread_token_totals(connection, schema)?;
    Ok(snapshot)
}

fn read_state_thread_token_totals(
    connection: &Connection,
    schema: &StateThreadsSchema,
) -> Result<Vec<ThreadTokenTotal>, SqlError> {
    let filter = schema
        .archived
        .as_ref()
        .map_or_else(String::new, |column| format!(" WHERE COALESCE({}, 0) = 0", ident(column)));
    let sql = format!(
        "SELECT {}, {}, {} FROM {}{} ORDER BY {} ASC",
        ident(&schema.id),
        ident(&schema.tokens_used),
        ident(&schema.updated_at_ms),
        ident(&schema.table.name),
        filter,
        ident(&schema.updated_at_ms),
    );
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map([], |row| {
            Ok(ThreadTokenTotal {
                thread_id: row.get(0)?,
                tokens_used: nonnegative(row.get::<_, Option<i64>>(1)?).unwrap_or_default(),
                updated_at_unix_ms: row.get::<_, Option<i64>>(2)?.unwrap_or_default(),
            })
        })?
        .collect()
}

fn read_thread_history_activity(
    connection: &Connection,
    schema: &ThreadHistoryActivitySchema,
    freshness_ttl: Duration,
    hard_ttl: Duration,
) -> Result<FallbackSnapshot, SqlError> {
    let mut snapshot = empty_snapshot();
    snapshot.activity_is_low_confidence = true;
    let Some(activity_record) = read_latest_in_progress_thread_history_turn(connection, schema)? else {
        return Ok(snapshot);
    };
    let now = now_unix_ms();
    let started_at_ms = normalize_unix_timestamp_ms(activity_record.started_at_ms);
    let activity_observed_at_ms = activity_record.heartbeat_at_ms.unwrap_or(started_at_ms);
    let ttl_ms = i64::try_from(freshness_ttl.as_millis()).unwrap_or(i64::MAX);
    let short_lease_until = activity_observed_at_ms.saturating_add(ttl_ms);
    let hard_ttl_ms = i64::try_from(hard_ttl.as_millis()).unwrap_or(i64::MAX);
    let hard_lease_until = started_at_ms.saturating_add(hard_ttl_ms);
    snapshot.activity_stale_after_unix_ms = Some(hard_lease_until);
    // 仅 inProgress 被视为低置信运行后备。安全 item 心跳仍优先决定精确阶段；
    // 心跳静默后，在有限租约内保持 Thinking，覆盖长推理的安静阶段。租约结束时
    // 主动回落 Idle：SQLite 没有收到终态时，不能让一条旧 inProgress 永远占用紫色。
    if activity_record.status.eq_ignore_ascii_case("inProgress") {
        if now <= short_lease_until {
            snapshot.activity = activity_from_safe_item_type(activity_record.latest_item_type.as_deref());
        } else if now <= hard_lease_until {
            snapshot.activity = ActivityState::Thinking;
        } else {
            snapshot.activity = ActivityState::Idle;
        }
    }
    Ok(snapshot)
}

/// 读取最新仍在进行中的 Turn，以及（若 schema 完整）其最近一次安全元数据心跳。
///
/// `thread_history` 可能将已完成 Turn 排在仍在执行的 Turn 前面；因此必须先按状态
/// 筛选 `inProgress`，而不是从全表的“最近一条”推断。只读取 thread_items 的关联键
/// 与 created_at_ms，刻意不触碰 `item_json`。
fn read_latest_in_progress_thread_history_turn(
    connection: &Connection,
    schema: &ThreadHistoryActivitySchema,
) -> Result<Option<ThreadHistoryActivityRecord>, SqlError> {
    let recent = schema.completed_at.as_ref().map_or_else(
        || ident(&schema.started_at),
        |completed_at| format!("COALESCE({}, {})", ident(completed_at), ident(&schema.started_at)),
    );
    let sql = format!(
        "SELECT {}, {} FROM {} WHERE LOWER({}) = 'inprogress' ORDER BY {} DESC LIMIT 1",
        ident(&schema.status),
        ident(&schema.started_at),
        ident(&schema.table.name),
        ident(&schema.status),
        recent
    );
    let active_turn =
        connection.query_row(&sql, [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))).optional()?;
    let Some((status, started_at)) = active_turn else {
        return Ok(None);
    };

    let (heartbeat_at_ms, item_type) = match (&schema.thread_id, &schema.turn_id, &schema.item) {
        (Some(thread_id), Some(turn_id), Some(item)) => {
            let identity_sql = format!(
                "SELECT {}, {} FROM {} WHERE LOWER({}) = 'inprogress' ORDER BY {} DESC LIMIT 1",
                ident(thread_id),
                ident(turn_id),
                ident(&schema.table.name),
                ident(&schema.status),
                recent
            );
            let identity = connection
                .query_row(&identity_sql, [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .optional()?;
            if let Some((thread_id, turn_id)) = identity {
                let heartbeat_sql = format!(
                    "SELECT {} FROM {} WHERE {} = ?1 AND {} = ?2 ORDER BY {} DESC LIMIT 1",
                    ident(&item.created_at_ms),
                    ident(&item.table.name),
                    ident(&item.thread_id),
                    ident(&item.turn_id),
                    ident(&item.created_at_ms),
                );
                let heartbeat = connection
                    .query_row(&heartbeat_sql, [&thread_id, &turn_id], |row| row.get::<_, i64>(0))
                    .optional()?;
                let item_type = item
                    .item_type
                    .as_ref()
                    .map(|item_type| {
                        let item_type_sql = format!(
                            "SELECT {} FROM {} WHERE {} = ?1 AND {} = ?2 ORDER BY {} DESC LIMIT 1",
                            ident(item_type),
                            ident(&item.table.name),
                            ident(&item.thread_id),
                            ident(&item.turn_id),
                            ident(&item.created_at_ms),
                        );
                        connection
                            .query_row(&item_type_sql, [&thread_id, &turn_id], |row| row.get::<_, String>(0))
                            .optional()
                    })
                    .transpose()?
                    .flatten();
                (heartbeat, item_type)
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    };
    Ok(Some(ThreadHistoryActivityRecord {
        status,
        started_at_ms: started_at,
        heartbeat_at_ms,
        latest_item_type: item_type,
    }))
}

/// 只从无正文的 `item_type` 推断当前阶段。缺失或未识别类型保持“推理中”，
/// 它表达已有活跃 Turn 但尚无更精确的安全证据，绝不读取 `item_json` 补猜。
fn activity_from_safe_item_type(item_type: Option<&str>) -> ActivityState {
    match item_type {
        Some("commandExecution")
        | Some("fileChange")
        | Some("webSearch")
        | Some("mcpToolCall")
        | Some("dynamicToolCall")
        | Some("subAgentActivity")
        | Some("collabAgentToolCall")
        | Some("imageView") => ActivityState::Executing,
        _ => ActivityState::Thinking,
    }
}

/// 将 Codex 历史库的 Unix 秒/毫秒时间戳统一为毫秒。
///
/// `thread_history_1.sqlite.thread_turns.started_at` 在当前 Codex 桌面版中使用
/// Unix 秒；其他已有后备表使用毫秒。小于 10^10 的正值在可预见的时间范围内
/// 只能是秒级 Unix 时间，因此安全地乘 1000；较大的值保持原样，避免二次放大。
const fn normalize_unix_timestamp_ms(timestamp: i64) -> i64 {
    if timestamp > 0 && timestamp < 10_000_000_000 { timestamp.saturating_mul(1_000) } else { timestamp }
}

fn read_token_totals(
    connection: &Connection,
    schema: &SupportedSchema,
    thread_id: &str,
) -> Result<TokenTotals, SqlError> {
    let expression = |column: &Option<String>| {
        column.as_ref().map_or_else(|| "NULL".to_owned(), |column| format!("SUM(COALESCE({}, 0))", ident(column)))
    };
    let sql = format!(
        "SELECT {}, {}, {}, {} FROM {} WHERE {} = ?1",
        expression(&schema.token_input),
        expression(&schema.token_output),
        expression(&schema.token_cached),
        expression(&schema.token_total),
        ident(&schema.token.name),
        ident(&schema.token_thread_id)
    );
    connection.query_row(&sql, [thread_id], |row| {
        Ok(TokenTotals {
            input: nonnegative(row.get::<_, Option<i64>>(0)?),
            output: nonnegative(row.get::<_, Option<i64>>(1)?),
            cached: nonnegative(row.get::<_, Option<i64>>(2)?),
            total: nonnegative(row.get::<_, Option<i64>>(3)?),
        })
    })
}

fn empty_snapshot() -> FallbackSnapshot {
    FallbackSnapshot {
        latest_thread_id: None,
        latest_turn_id: None,
        latest_item_id: None,
        activity: ActivityState::Unknown,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        total_tokens: None,
        raw_thread_tokens_used: None,
        thread_token_totals: Vec::new(),
        is_stale_fallback: true,
        activity_is_low_confidence: false,
        activity_stale_after_unix_ms: None,
        observed_at_unix_ms: now_unix_ms(),
    }
}

fn inspect_schema(connection: &Connection) -> Result<SchemaInspection, SqlError> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let names = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({})", ident(&name)))?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
        tables.push(TableInspection { name, columns });
    }
    Ok(SchemaInspection { tables })
}

fn candidate_table(tables: &BTreeMap<String, TableLayout>, candidates: &[&str]) -> Option<TableLayout> {
    candidates.iter().find_map(|name| tables.get(*name).cloned())
}

fn required_column(
    table: &TableLayout,
    candidates: &[&str],
    capability: &str,
    missing: &mut Vec<String>,
) -> Option<String> {
    let value = optional_column(table, candidates);
    if value.is_none() {
        missing.push(format!("{}（表 {}）", capability, table.name));
    }
    value
}

fn optional_column(table: &TableLayout, candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|name| table.columns.get(*name).cloned())
}

fn activity_from_status(status: Option<&str>) -> ActivityState {
    let value = status.unwrap_or_default().to_ascii_lowercase();
    if ["failed", "error"].contains(&value.as_str()) {
        ActivityState::Failed
    } else if ["waiting_for_user", "waiting", "requires_input", "approval_required"].contains(&value.as_str()) {
        ActivityState::WaitingForUser
    } else if ["executing", "running", "in_progress"].contains(&value.as_str()) {
        ActivityState::Executing
    } else if ["thinking", "started", "active"].contains(&value.as_str()) {
        ActivityState::Thinking
    } else if ["completed", "complete", "succeeded"].contains(&value.as_str()) {
        ActivityState::Completed
    } else if ["idle"].contains(&value.as_str()) {
        ActivityState::Idle
    } else {
        ActivityState::Unknown
    }
}

fn nonnegative(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

fn now_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| value.as_millis().try_into().unwrap_or(i64::MAX))
}

fn ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn readonly_uri(path: &Path, immutable: ImmutableMode) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let normalized = if raw.starts_with('/') { raw } else { format!("/{raw}") };
    let encoded = normalized.bytes().fold(String::new(), |mut output, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'.' | b'-' | b'_') {
            output.push(byte as char);
        } else {
            use fmt::Write as _;
            let _ = write!(output, "%{byte:02X}");
        }
        output
    });
    let immutable = matches!(immutable, ImmutableMode::Explicit).then_some("&immutable=1").unwrap_or("");
    format!("file://{encoded}?mode=ro{immutable}")
}

fn classify_error(error: &SqlError) -> FallbackFailure {
    match error {
        SqlError::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) =>
        {
            FallbackFailure::Busy
        }
        SqlError::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) =>
        {
            FallbackFailure::Corrupt
        }
        _ => FallbackFailure::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    fn temp_database() -> PathBuf {
        std::env::temp_dir().join(format!(
            "codex-taskbar-sqlite-test-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn adapter(path: &Path) -> CodexSqliteFallback {
        CodexSqliteFallback::new(SqliteFallbackConfig {
            database_path: path.to_owned(),
            busy_timeout: Duration::from_millis(25),
            activity_freshness_ttl: Duration::from_secs(30),
            activity_hard_ttl: Duration::from_secs(12 * 60 * 60),
            immutable: ImmutableMode::Disabled,
        })
    }

    fn supported_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, updated_at INTEGER NOT NULL); CREATE TABLE turns (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, status TEXT, updated_at INTEGER NOT NULL); CREATE TABLE items (id TEXT PRIMARY KEY, turn_id TEXT NOT NULL, status TEXT, updated_at INTEGER NOT NULL); CREATE TABLE token_usage (thread_id TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER, cached_input_tokens INTEGER, total_tokens INTEGER);").unwrap();
        connection.execute("INSERT INTO threads VALUES ('old', 1), ('recent', 2)", []).unwrap();
        connection
            .execute(
                "INSERT INTO turns VALUES ('t-old', 'old', 'completed', 1), ('t-new', 'recent', 'thinking', 3)",
                [],
            )
            .unwrap();
        connection.execute("INSERT INTO items VALUES ('i-new', 't-new', 'executing', 4)", []).unwrap();
        connection
            .execute("INSERT INTO token_usage VALUES ('recent', 10, 3, 2, 15), ('recent', 20, 5, 4, 29)", [])
            .unwrap();
        connection
    }

    #[test]
    fn reads_explicit_supported_schema_and_aggregates_latest_thread() {
        let path = temp_database();
        let connection = supported_database(&path);
        let report = adapter(&path).read_snapshot();
        assert!(matches!(report.health, FallbackHealth::Available { .. }));
        let snapshot = report.snapshot.unwrap();
        assert_eq!(snapshot.latest_thread_id.as_deref(), Some("recent"));
        assert_eq!(snapshot.latest_turn_id.as_deref(), Some("t-new"));
        assert_eq!(snapshot.latest_item_id.as_deref(), Some("i-new"));
        assert_eq!(snapshot.activity, ActivityState::Executing);
        assert_eq!(
            (snapshot.input_tokens, snapshot.output_tokens, snapshot.cached_input_tokens, snapshot.total_tokens),
            (Some(30), Some(8), Some(6), Some(44))
        );
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_columns_return_structured_unsupported_schema() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT); CREATE TABLE turns (id TEXT); CREATE TABLE token_usage (thread_id TEXT);").unwrap();
        let report = adapter(&path).read_snapshot();
        assert!(matches!(report.health, FallbackHealth::UnsupportedSchema { .. }));
        assert!(report.snapshot.is_none());
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn state_threads_schema_returns_only_raw_stale_tokens() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, updated_at_ms INTEGER NOT NULL, tokens_used INTEGER, archived INTEGER); INSERT INTO threads VALUES ('visible', 10, 123456, 0), ('archived', 20, 999999, 1);",
            )
            .unwrap();
        let report = adapter(&path).read_snapshot();
        assert!(matches!(report.health, FallbackHealth::Available { .. }));
        let snapshot = report.snapshot.unwrap();
        assert_eq!(snapshot.latest_thread_id.as_deref(), Some("visible"));
        assert_eq!(snapshot.raw_thread_tokens_used, Some(123456));
        assert_eq!(
            snapshot
                .thread_token_totals
                .iter()
                .map(|total| (total.thread_id.as_str(), total.tokens_used))
                .collect::<Vec<_>>(),
            [("visible", 123456)]
        );
        assert_eq!(snapshot.total_tokens, None);
        assert!(snapshot.is_stale_fallback);
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn thread_history_in_progress_expires_after_freshness_ttl_without_reading_item_json() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        let started_at = now_unix_ms();
        connection
            .execute_batch("CREATE TABLE thread_turns (status TEXT, started_at INTEGER, completed_at INTEGER); CREATE TABLE thread_items (item_json TEXT, item_type TEXT);")
            .unwrap();
        connection.execute("INSERT INTO thread_turns VALUES ('inProgress', ?1, NULL)", [started_at]).unwrap();
        connection
            .execute("INSERT INTO thread_items VALUES ('{\"secret\":\"must not be read\"}', 'message')", [])
            .unwrap();
        let report = adapter(&path).read_snapshot();
        let snapshot = report.snapshot.unwrap();
        assert_eq!(snapshot.activity, ActivityState::Thinking);
        assert!(snapshot.activity_is_low_confidence);
        assert!(snapshot.activity_stale_after_unix_ms.unwrap() >= started_at);
        drop(connection);
        fs::remove_file(path).unwrap();

        let stale_path = temp_database();
        let stale = Connection::open(&stale_path).unwrap();
        stale
            .execute_batch("CREATE TABLE thread_turns (status TEXT, started_at INTEGER, completed_at INTEGER);")
            .unwrap();
        stale.execute("INSERT INTO thread_turns VALUES ('inProgress', 1, NULL)", []).unwrap();
        assert_eq!(adapter(&stale_path).read_snapshot().snapshot.unwrap().activity, ActivityState::Idle);
        drop(stale);
        fs::remove_file(stale_path).unwrap();
    }

    #[test]
    fn thread_history_in_progress_with_unix_seconds_timestamp_is_currently_active() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        // Codex 当前真实 thread_history_1.sqlite 使用的是 Unix 秒，而不是
        // Unix 毫秒。若直接与 `now_unix_ms` 比较，会把正在执行的 Turn 错判为
        // 约 1970 年的陈旧记录。
        let started_at_seconds = now_unix_ms() / 1_000;
        connection
            .execute_batch("CREATE TABLE thread_turns (status TEXT, started_at INTEGER, completed_at INTEGER);")
            .unwrap();
        connection.execute("INSERT INTO thread_turns VALUES ('inProgress', ?1, NULL)", [started_at_seconds]).unwrap();

        let snapshot = adapter(&path).read_snapshot().snapshot.expect("只读后备应返回活动快照");

        assert_eq!(snapshot.activity, ActivityState::Thinking);
        assert!(snapshot.activity_stale_after_unix_ms.expect("应使用毫秒时间戳") >= now_unix_ms());
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn thread_history_in_progress_survives_a_quiet_long_running_phase_until_hard_lease() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        // 定义自定义交互组件、长推理等阶段可能超过 30 秒没有新 item；只要 Turn
        // 仍是 inProgress，短心跳租约失效后仍应以低置信 Thinking 保持活动。
        let started_at = now_unix_ms().saturating_sub(90_000);
        connection
            .execute_batch("CREATE TABLE thread_turns (status TEXT, started_at INTEGER, completed_at INTEGER);")
            .unwrap();
        connection.execute("INSERT INTO thread_turns VALUES ('inProgress', ?1, NULL)", [started_at]).unwrap();

        let snapshot = adapter(&path).read_snapshot().snapshot.expect("应返回活动快照");

        assert_eq!(snapshot.activity, ActivityState::Thinking);
        assert!(snapshot.activity_stale_after_unix_ms.expect("应提供硬租约") > now_unix_ms());
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn thread_history_in_progress_past_hard_lease_returns_to_idle() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        let started_at = now_unix_ms().saturating_sub(2_000);
        connection
            .execute_batch("CREATE TABLE thread_turns (status TEXT, started_at INTEGER, completed_at INTEGER);")
            .unwrap();
        connection.execute("INSERT INTO thread_turns VALUES ('inProgress', ?1, NULL)", [started_at]).unwrap();

        let fallback = CodexSqliteFallback::new(SqliteFallbackConfig {
            database_path: path.clone(),
            busy_timeout: Duration::from_millis(25),
            activity_freshness_ttl: Duration::from_millis(10),
            activity_hard_ttl: Duration::from_millis(20),
            immutable: ImmutableMode::Disabled,
        });
        let snapshot = fallback.read_snapshot().snapshot.expect("应返回活动快照");

        assert_eq!(snapshot.activity, ActivityState::Idle);
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn thread_history_uses_safe_item_timestamp_as_heartbeat_for_long_running_turn() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        // 一个真实的长任务可能早于活动 TTL 开始，但仍持续写入 thread_items。
        // 适配器只能读取关联键和 created_at_ms，绝不读取 item_json 正文。
        let stale_started_at = now_unix_ms().saturating_sub(60_000);
        let fresh_heartbeat_at = now_unix_ms();
        connection
            .execute_batch(
                "CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER);\
                 CREATE TABLE thread_items (thread_id TEXT, turn_id TEXT, item_id TEXT, created_at_ms INTEGER, item_json TEXT);",
            )
            .unwrap();
        connection
            .execute("INSERT INTO thread_turns VALUES ('thread', 'turn', 'inProgress', ?1, NULL)", [stale_started_at])
            .unwrap();
        connection
            .execute(
                "INSERT INTO thread_items VALUES ('thread', 'turn', 'item', ?1, '{\"secret\":\"must not be read\"}')",
                [fresh_heartbeat_at],
            )
            .unwrap();

        let snapshot = adapter(&path).read_snapshot().snapshot.expect("只读后备应返回活动快照");

        assert_eq!(snapshot.activity, ActivityState::Thinking);
        assert!(snapshot.activity_stale_after_unix_ms.expect("应以最近安全心跳计算 TTL") >= fresh_heartbeat_at);
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn thread_history_prefers_a_fresh_in_progress_turn_over_completed_turn_with_same_timestamp() {
        let path = temp_database();
        let connection = Connection::open(&path).unwrap();
        let now = now_unix_ms();
        connection
            .execute_batch(
                "CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER);\
                 CREATE TABLE thread_items (thread_id TEXT, turn_id TEXT, item_id TEXT, created_at_ms INTEGER);",
            )
            .unwrap();
        connection
            .execute("INSERT INTO thread_turns VALUES ('done-thread', 'done-turn', 'completed', ?1, ?1)", [now])
            .unwrap();
        connection
            .execute("INSERT INTO thread_turns VALUES ('active-thread', 'active-turn', 'inProgress', ?1, NULL)", [now])
            .unwrap();
        connection
            .execute("INSERT INTO thread_items VALUES ('active-thread', 'active-turn', 'item', ?1)", [now])
            .unwrap();

        let snapshot = adapter(&path).read_snapshot().snapshot.expect("只读后备应返回活动快照");

        assert_eq!(snapshot.activity, ActivityState::Thinking);
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn safe_item_type_distinguishes_tool_execution_without_reading_item_json() {
        assert_eq!(activity_from_safe_item_type(Some("reasoning")), ActivityState::Thinking);
        assert_eq!(activity_from_safe_item_type(Some("commandExecution")), ActivityState::Executing);
        assert_eq!(activity_from_safe_item_type(Some("fileChange")), ActivityState::Executing);
        assert_eq!(activity_from_safe_item_type(None), ActivityState::Thinking);
    }

    #[test]
    fn locked_database_is_reported_without_falling_back_to_write_access() {
        let path = temp_database();
        let connection = supported_database(&path);
        connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let report = adapter(&path).read_snapshot();
        assert_eq!(report.health, FallbackHealth::Unavailable { failure: FallbackFailure::Busy });
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_database_is_reported() {
        let path = temp_database();
        fs::write(&path, b"not a sqlite database").unwrap();
        let report = adapter(&path).read_snapshot();
        assert_eq!(report.health, FallbackHealth::Unavailable { failure: FallbackFailure::Corrupt });
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn readonly_uri_and_read_do_not_create_or_change_schema() {
        let path = temp_database();
        let connection = supported_database(&path);
        let before: i64 = connection.query_row("PRAGMA schema_version", [], |row| row.get(0)).unwrap();
        let report = adapter(&path).read_snapshot();
        assert!(report.snapshot.is_some());
        let after: i64 = connection.query_row("PRAGMA schema_version", [], |row| row.get(0)).unwrap();
        assert_eq!(before, after);
        assert!(readonly_uri(&path, ImmutableMode::Disabled).contains("mode=ro"));
        drop(connection);
        fs::remove_file(path).unwrap();
    }
}

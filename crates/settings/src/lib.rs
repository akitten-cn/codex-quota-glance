//! 官方登录版的持久化任务栏设置。
//!
//! 配置只保存窗口布局、展示项目、日志和 Codex CLI 路径。旧版 New API 字段会被
//! serde 安全忽略，并在下一次保存时移除；本模块不再读取、保存或迁移任何 API 凭据。

use std::{fmt, fs, io, path::Path, str::FromStr};

pub use codex_taskbar_domain::layout::{DisplayItemKind, TaskbarAnchor};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

/// 当前官方登录配置文件格式版本。
pub const CONFIG_VERSION: u32 = 9;
/// 设置进程写入、常驻进程消费的无敏感信息重载标记文件名。
pub const RELOAD_MARKER_FILE: &str = "settings.reload";

/// 叠浪胶囊需要同时容纳今日、缓存、Credits 与双额度文字。旧版 320px 会让
/// 这些信息彼此压缩。经真实任务栏截图对照后，520px 在常见缩放下仍偏长；
/// 新安装默认使用 440px，保持数据可读而不占据过多任务栏空间。
const DEFAULT_WIDTH_PX: u32 = 440;
/// 200px 是用户允许的最窄胶囊；窄宽模式会折叠次要的缓存与 5 小时文字。
/// 超过 620px 会明显侵占任务栏。
pub const MIN_TASKBAR_WIDTH_PX: u32 = 200;
pub const MAX_TASKBAR_WIDTH_PX: u32 = 620;
const MAX_SAFE_SPACING_PX: u32 = 1_024;
const MAX_TRAFFIC_MONITOR_OFFSET_PX: i32 = 4_096;
const DEFAULT_TASKBAR_BACKGROUND_OPACITY_PERCENT: u8 = 70;
const MIN_TASKBAR_BACKGROUND_OPACITY_PERCENT: u8 = 20;

/// 配置文件允许的日志等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// 官方数据的后台校验策略。事件更新不受此项影响；它只控制静默时的后备检查频率。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    #[default]
    Smart,
    Economy,
}

impl LogLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        if cfg!(debug_assertions) { Self::Debug } else { Self::Info }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for LogLevel {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// 无法识别的日志等级。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("未知日志等级：{0}")]
pub struct InvalidLogLevel(String);

impl FromStr for LogLevel {
    type Err = InvalidLogLevel;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Ok(Self::Off),
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" | "information" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(InvalidLogLevel(value.to_owned())),
        }
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// 一个可见性和顺序可配置的任务栏组件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayItemSetting {
    pub kind: DisplayItemKind,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub order: u16,
    #[serde(default = "default_min_width")]
    pub min_width_px: u16,
    #[serde(default = "default_keep_priority")]
    pub keep_priority: u8,
}

impl Default for DisplayItemSetting {
    fn default() -> Self {
        Self { kind: DisplayItemKind::ActivityLight, visible: true, order: 0, min_width_px: 16, keep_priority: 30 }
    }
}

/// 与领域布局模型对接的兼容短名称。
pub type DisplayItemConfig = DisplayItemSetting;

/// 官方登录运行模式使用的全部设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_version", alias = "schema_version")]
    pub version: u32,
    #[serde(default, alias = "monitor_device_name")]
    pub target_monitor_device: Option<String>,
    /// 未指定固定设备名时是否优先选择副屏；关闭后优先主屏。保留独立布尔值
    /// 而不是用伪设备名编码，显示器重连后仍能安全回退。
    #[serde(default = "default_true")]
    pub prefer_secondary_monitor: bool,
    #[serde(default = "default_anchor")]
    pub anchor: TaskbarAnchor,
    #[serde(default = "default_width", alias = "preferred_width_px")]
    pub taskbar_width_px: u32,
    #[serde(default = "default_spacing", alias = "edge_gap_px")]
    pub safe_spacing_px: u32,
    #[serde(default, alias = "traffic_monitor_reserved_offset_px", alias = "reserved_offset_px")]
    pub traffic_monitor_offset_px: i32,
    #[serde(default = "default_display_items")]
    pub display_items: Vec<DisplayItemSetting>,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub reduce_motion: bool,
    /// 官方事件静默时的后备同步策略。
    #[serde(default)]
    pub sync_mode: SyncMode,
    /// 本机聚合用量账本的保留天数，不包含 Prompt、回复或线程内容。
    #[serde(default = "default_history_retention_days")]
    pub history_retention_days: u16,
    /// 诊断日志的保留天数。
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u16,
    /// 更新包较大且服务端支持 Range 时是否允许自适应分块下载。
    #[serde(default = "default_true")]
    pub adaptive_chunk_download: bool,
    /// 未消耗区域的深色玻璃不透明度。70% 让桌面略微透出，同时保证任务栏
    /// 白色文字仍可读；仅保存用户设置，不参与任何遥测或高频写入。
    #[serde(default = "default_taskbar_background_opacity_percent")]
    pub taskbar_background_opacity_percent: u8,
    #[serde(default, alias = "codex_path")]
    pub codex_cli_path: Option<String>,
}

pub type Settings = AppConfig;
pub type Config = AppConfig;

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            target_monitor_device: None,
            prefer_secondary_monitor: true,
            anchor: TaskbarAnchor::Right,
            taskbar_width_px: DEFAULT_WIDTH_PX,
            safe_spacing_px: 8,
            traffic_monitor_offset_px: 0,
            display_items: default_display_items(),
            log_level: LogLevel::default(),
            reduce_motion: false,
            sync_mode: SyncMode::Smart,
            history_retention_days: default_history_retention_days(),
            log_retention_days: default_log_retention_days(),
            adaptive_chunk_download: true,
            taskbar_background_opacity_percent: DEFAULT_TASKBAR_BACKGROUND_OPACITY_PERCENT,
            codex_cli_path: None,
        }
    }
}

impl AppConfig {
    /// 读取 JSON 后规范化。未知字段会被忽略，以便旧配置无痛迁移到官方登录版。
    pub fn from_json(contents: &str) -> Result<Self, ConfigError> {
        serde_json::from_str::<Self>(contents).map(Self::normalize).map_err(ConfigError::Parse)
    }

    pub fn to_json(&self) -> Result<String, ConfigError> {
        serde_json::to_string_pretty(&self.normalized()).map_err(ConfigError::Serialize)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let database = settings_database_path(path).ok_or_else(|| ConfigError::Read {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "配置路径缺少父目录"),
        })?;
        if database.is_file() {
            let connection = open_settings_database(&database)?;
            if let Some(payload) = connection
                .query_row("SELECT payload_json FROM settings_snapshot WHERE id = 1", [], |row| row.get::<_, String>(0))
                .optional()
                .map_err(|source| ConfigError::Database { path: database.clone(), source })?
            {
                return Self::from_json(&payload);
            }
        }

        // 一次性兼容旧版 settings.json。成功导入后 SQLite 成为唯一写入目标；
        // 旧文件只作为用户可恢复的迁移备份保留，不再参与后续读取。
        let imported = fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_owned(), source })
            .and_then(|value| Self::from_json(&value))?;
        imported.save_atomic(path)?;
        Ok(imported)
    }

    /// 缺失时创建默认配置；不触及旧 New API 凭据侧车文件。
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Ok(config) => Ok(config),
            Err(ConfigError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                let config = Self::default();
                config.save_atomic(path)?;
                Ok(config)
            }
            Err(error) => Err(error),
        }
    }

    /// 在 SQLite 事务中写入一个版本化设置快照。设置是低频、整体提交的聚合根，
    /// 因此保留 JSON payload 以支持字段前向兼容；时序用量采用独立关系表。
    pub fn save_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        let parent = path.parent().ok_or_else(|| ConfigError::Write {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "配置路径缺少父目录"),
        })?;
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write { path: parent.to_owned(), source })?;
        let database = settings_database_path(path).expect("已验证设置父目录");
        let mut connection = open_settings_database(&database)?;
        let transaction =
            connection.transaction().map_err(|source| ConfigError::Database { path: database.clone(), source })?;
        transaction
            .execute(
                "INSERT INTO settings_snapshot(id, schema_version, payload_json, updated_at_unix) \
                 VALUES(1, ?1, ?2, unixepoch()) \
                 ON CONFLICT(id) DO UPDATE SET schema_version=excluded.schema_version, \
                 payload_json=excluded.payload_json, updated_at_unix=excluded.updated_at_unix",
                params![CONFIG_VERSION, self.to_json()?],
            )
            .map_err(|source| ConfigError::Database { path: database.clone(), source })?;
        transaction.commit().map_err(|source| ConfigError::Database { path: database, source })
    }

    /// 规范化手工编辑的边界值和重复显示项。
    #[must_use]
    pub fn normalize(mut self) -> Self {
        // v8 设置页把 JSON 快照错误注入成字符串，页面始终误判为“无副屏”，保存时
        // 会把默认的自动副屏偏好覆盖为 false。该值无法代表用户真实选择，迁移时恢复默认。
        if self.version < 9 {
            self.prefer_secondary_monitor = true;
        }
        // v3 的 320px 是早期圆环任务栏的默认值，不是用户针对叠浪版的显式设计。
        // 仅在版本迁移且值恰好等于旧默认时升级；其它手工宽度绝不改写。
        if self.version < 4 && self.taskbar_width_px == 320 {
            self.taskbar_width_px = DEFAULT_WIDTH_PX;
        }
        // v4 的 620px 和 v5 的 520px 都是本项目曾经的默认宽度。v6 根据实际
        // 任务栏截图再收紧；只迁移恰好等于旧默认的值，其他显式选择保持不变。
        if self.version < CONFIG_VERSION && self.taskbar_width_px == 620 {
            self.taskbar_width_px = DEFAULT_WIDTH_PX;
        }
        if self.version < CONFIG_VERSION && self.taskbar_width_px == 520 {
            self.taskbar_width_px = DEFAULT_WIDTH_PX;
        }
        self.version = CONFIG_VERSION;
        self.target_monitor_device = trim_non_empty(self.target_monitor_device);
        self.codex_cli_path = trim_non_empty(self.codex_cli_path);
        self.taskbar_width_px = self.taskbar_width_px.clamp(MIN_TASKBAR_WIDTH_PX, MAX_TASKBAR_WIDTH_PX);
        self.safe_spacing_px = self.safe_spacing_px.min(MAX_SAFE_SPACING_PX);
        self.traffic_monitor_offset_px = self.traffic_monitor_offset_px.clamp(0, MAX_TRAFFIC_MONITOR_OFFSET_PX);
        self.taskbar_background_opacity_percent =
            self.taskbar_background_opacity_percent.clamp(MIN_TASKBAR_BACKGROUND_OPACITY_PERCENT, 100);
        self.history_retention_days = match self.history_retention_days {
            30 | 90 | 365 => self.history_retention_days,
            _ => default_history_retention_days(),
        };
        self.log_retention_days = match self.log_retention_days {
            7 | 30 | 90 => self.log_retention_days,
            _ => default_log_retention_days(),
        };
        let mut seen = Vec::new();
        self.display_items.retain(|item| {
            if seen.contains(&item.kind) {
                false
            } else {
                seen.push(item.kind);
                true
            }
        });
        if self.display_items.is_empty() {
            self.display_items = default_display_items();
        }
        self
    }

    #[must_use]
    pub fn normalized(&self) -> Self {
        self.clone().normalize()
    }
    #[must_use]
    pub const fn preferred_width_px(&self) -> u32 {
        self.taskbar_width_px
    }
    #[must_use]
    pub const fn reserved_offset_px(&self) -> i32 {
        self.traffic_monitor_offset_px
    }
    #[must_use]
    pub const fn edge_gap_px(&self) -> u32 {
        self.safe_spacing_px
    }

    #[must_use]
    pub const fn taskbar_background_opacity(&self) -> f32 {
        self.taskbar_background_opacity_percent as f32 / 100.0
    }
}

const fn default_taskbar_background_opacity_percent() -> u8 {
    DEFAULT_TASKBAR_BACKGROUND_OPACITY_PERCENT
}

const fn default_history_retention_days() -> u16 {
    90
}

const fn default_log_retention_days() -> u16 {
    30
}

/// 返回与配置文件同目录的重载标记路径。
#[must_use]
pub fn reload_marker_path(settings_path: &Path) -> Option<std::path::PathBuf> {
    settings_path.parent().map(|directory| directory.join(RELOAD_MARKER_FILE))
}

/// 请求常驻进程重新读取设置。标记仅表示“配置已保存”，不携带路径、账户或凭据。
pub fn request_reload(settings_path: &Path) -> Result<(), ConfigError> {
    let marker = reload_marker_path(settings_path).ok_or_else(|| ConfigError::Write {
        path: settings_path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "配置路径缺少父目录"),
    })?;
    fs::write(&marker, b"reload\n").map_err(|source| ConfigError::Write { path: marker, source })
}

/// 消费一次重载请求。删除失败时保持标记，下个低频检查会继续尝试，避免丢失更新。
pub fn consume_reload_request(settings_path: &Path) -> Result<bool, ConfigError> {
    let Some(marker) = reload_marker_path(settings_path) else { return Ok(false) };
    if !marker.exists() {
        return Ok(false);
    }
    fs::remove_file(&marker).map_err(|source| ConfigError::Write { path: marker, source })?;
    Ok(true)
}

/// 配置 I/O 或 JSON 失败。
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("读取配置失败（{path}）：{source}")]
    Read { path: std::path::PathBuf, source: io::Error },
    #[error("写入配置失败（{path}）：{source}")]
    Write { path: std::path::PathBuf, source: io::Error },
    #[error("配置 JSON 无效：{0}")]
    Parse(serde_json::Error),
    #[error("配置 JSON 序列化失败：{0}")]
    Serialize(serde_json::Error),
    #[error("配置数据库失败（{path}）：{source}")]
    Database { path: std::path::PathBuf, source: rusqlite::Error },
}

/// 设置与本机聚合用量共用一个应用数据库，表之间没有敏感外键。
#[must_use]
pub fn settings_database_path(settings_path: &Path) -> Option<std::path::PathBuf> {
    settings_path.parent().map(|directory| directory.join("codex-taskbar.db"))
}

fn open_settings_database(path: &Path) -> Result<Connection, ConfigError> {
    let connection =
        Connection::open(path).map_err(|source| ConfigError::Database { path: path.to_owned(), source })?;
    connection
        .busy_timeout(std::time::Duration::from_secs(3))
        .map_err(|source| ConfigError::Database { path: path.to_owned(), source })?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS settings_snapshot(
               id INTEGER PRIMARY KEY CHECK(id=1),
               schema_version INTEGER NOT NULL,
               payload_json TEXT NOT NULL,
               updated_at_unix INTEGER NOT NULL
             );",
        )
        .map_err(|source| ConfigError::Database { path: path.to_owned(), source })?;
    Ok(connection)
}

fn trim_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}
fn default_version() -> u32 {
    CONFIG_VERSION
}
fn default_width() -> u32 {
    DEFAULT_WIDTH_PX
}
fn default_spacing() -> u32 {
    8
}
fn default_anchor() -> TaskbarAnchor {
    TaskbarAnchor::Right
}
const fn default_true() -> bool {
    true
}
const fn default_min_width() -> u16 {
    24
}
const fn default_keep_priority() -> u8 {
    10
}

fn default_display_items() -> Vec<DisplayItemSetting> {
    vec![
        DisplayItemSetting {
            kind: DisplayItemKind::ActivityLight,
            visible: true,
            order: 0,
            min_width_px: 16,
            keep_priority: 30,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::QuotaRings,
            visible: true,
            order: 1,
            min_width_px: 46,
            keep_priority: 29,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::ResetCountdown,
            visible: true,
            order: 2,
            min_width_px: 52,
            keep_priority: 15,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::TodayTokens,
            visible: true,
            order: 3,
            min_width_px: 62,
            keep_priority: 18,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::CacheHitRate,
            visible: true,
            order: 4,
            min_width_px: 58,
            keep_priority: 20,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::CurrentThreadTokens,
            visible: false,
            order: 5,
            min_width_px: 58,
            keep_priority: 8,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::InputTokens,
            visible: false,
            order: 6,
            min_width_px: 54,
            keep_priority: 6,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::OutputTokens,
            visible: false,
            order: 7,
            min_width_px: 54,
            keep_priority: 6,
        },
        DisplayItemSetting {
            kind: DisplayItemKind::DataFreshness,
            visible: false,
            order: 8,
            min_width_px: 40,
            keep_priority: 2,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn old_new_api_fields_are_ignored_by_the_official_only_schema() {
        let config = AppConfig::from_json(r#"{"new_api":{"api_key":"must-not-load"},"taskbar_width_px":300}"#).unwrap();
        assert_eq!(config.taskbar_width_px, 300);
        assert!(!config.to_json().unwrap().contains("new_api"));
    }
    #[test]
    fn normalization_keeps_each_display_item_once() {
        let mut config = AppConfig::default();
        config.display_items.push(config.display_items[0].clone());
        assert_eq!(config.normalize().display_items.len(), default_display_items().len());
    }

    #[test]
    fn legacy_default_widths_are_compacted_but_custom_width_is_preserved() {
        let compacted = AppConfig::from_json(r#"{"version":4,"taskbar_width_px":620}"#).unwrap();
        assert_eq!(compacted.version, CONFIG_VERSION);
        assert_eq!(compacted.taskbar_width_px, DEFAULT_WIDTH_PX);

        let custom = AppConfig::from_json(r#"{"version":4,"taskbar_width_px":600}"#).unwrap();
        assert_eq!(custom.taskbar_width_px, 600);

        let v5_default = AppConfig::from_json(r#"{"version":5,"taskbar_width_px":520}"#).unwrap();
        assert_eq!(v5_default.taskbar_width_px, DEFAULT_WIDTH_PX);
    }

    #[test]
    fn v8_broken_settings_snapshot_migrates_back_to_auto_secondary() {
        let migrated =
            AppConfig::from_json(r#"{"version":8,"prefer_secondary_monitor":false,"taskbar_width_px":420}"#).unwrap();
        assert_eq!(migrated.version, CONFIG_VERSION);
        assert!(migrated.prefer_secondary_monitor);

        let explicit_v9 =
            AppConfig::from_json(r#"{"version":9,"prefer_secondary_monitor":false,"taskbar_width_px":420}"#).unwrap();
        assert!(!explicit_v9.prefer_secondary_monitor);
    }

    #[test]
    fn taskbar_background_opacity_defaults_and_clamps_to_readable_glass_range() {
        let default_config = AppConfig::from_json("{}").unwrap();
        assert_eq!(default_config.taskbar_background_opacity_percent, 70);
        assert!((default_config.taskbar_background_opacity() - 0.70).abs() < f32::EPSILON);

        let low = AppConfig::from_json(r#"{"taskbar_background_opacity_percent": 1}"#).unwrap();
        assert_eq!(low.taskbar_background_opacity_percent, 20);
        let high = AppConfig::from_json(r#"{"taskbar_background_opacity_percent": 255}"#).unwrap();
        assert_eq!(high.taskbar_background_opacity_percent, 100);
    }

    #[test]
    fn operational_settings_default_and_normalize_to_supported_values() {
        let defaults = AppConfig::from_json("{}").unwrap();
        assert_eq!(defaults.sync_mode, SyncMode::Smart);
        assert_eq!(defaults.history_retention_days, 90);
        assert_eq!(defaults.log_retention_days, 30);
        assert!(defaults.adaptive_chunk_download);

        let normalized = AppConfig::from_json(
            r#"{"history_retention_days":45,"log_retention_days":365,"adaptive_chunk_download":false}"#,
        )
        .unwrap();
        assert_eq!(normalized.history_retention_days, 90);
        assert_eq!(normalized.log_retention_days, 30);
        assert!(!normalized.adaptive_chunk_download);
    }

    #[test]
    fn manual_layout_values_clamp_to_non_negative_bounded_contract() {
        let low = AppConfig::from_json(r#"{"taskbar_width_px":1,"traffic_monitor_offset_px":-99}"#).unwrap();
        assert_eq!(low.taskbar_width_px, MIN_TASKBAR_WIDTH_PX);
        assert_eq!(low.traffic_monitor_offset_px, 0);
        let high = AppConfig::from_json(r#"{"version":8,"taskbar_width_px":9999}"#).unwrap();
        assert_eq!(high.taskbar_width_px, MAX_TASKBAR_WIDTH_PX);
    }

    #[test]
    fn settings_are_persisted_in_sqlite_and_survive_a_fresh_load() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("codex-taskbar-settings-db-{nonce}"));
        let settings_path = directory.join("settings.json");
        let mut saved = AppConfig { taskbar_width_px: 200, prefer_secondary_monitor: false, ..AppConfig::default() };
        saved.safe_spacing_px = 19;

        saved.save_atomic(&settings_path).unwrap();
        assert!(!settings_path.exists());
        assert!(settings_database_path(&settings_path).unwrap().is_file());
        assert_eq!(AppConfig::load(&settings_path).unwrap(), saved.normalized());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_json_is_imported_once_and_sqlite_becomes_authoritative() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("codex-taskbar-settings-import-{nonce}"));
        let settings_path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&settings_path, r#"{"version":9,"taskbar_width_px":240,"safe_spacing_px":11}"#).unwrap();

        let imported = AppConfig::load(&settings_path).unwrap();
        assert_eq!(imported.taskbar_width_px, 240);
        assert_eq!(imported.safe_spacing_px, 11);
        fs::write(&settings_path, r#"{"version":9,"taskbar_width_px":620}"#).unwrap();
        assert_eq!(AppConfig::load(&settings_path).unwrap().taskbar_width_px, 240);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reload_marker_is_consumed_exactly_once_without_configuration_payload() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("codex-taskbar-settings-{nonce}"));
        let settings_path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();

        request_reload(&settings_path).unwrap();
        assert!(reload_marker_path(&settings_path).unwrap().exists());
        assert!(consume_reload_request(&settings_path).unwrap());
        assert!(!consume_reload_request(&settings_path).unwrap());

        fs::remove_dir_all(directory).unwrap();
    }
}

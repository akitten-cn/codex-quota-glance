//! App Server 额度响应的兼容解析。
//!
//! 服务端把短窗口和长窗口分别称为 `primary`、`secondary`，但这两个名字不是
//! 时间窗口的契约。因此解析器只信任 `windowDurationMins`，未知窗口保留在原始
//! 响应中但不会误绘制成 5h 或 weekly。

use codex_taskbar_application::RateLimitSnapshot;
use codex_taskbar_domain::quota::QuotaValue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// 解析额度时的输入错误；未知窗口不是错误，只会被忽略分类。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RateLimitError {
    #[error("额度响应不是合法 JSON：{0}")]
    InvalidJson(String),
    #[error("额度响应缺少 rateLimits、primary 或 secondary")]
    MissingRateLimits,
    #[error("额度窗口不是对象")]
    InvalidWindow,
    #[error("额度窗口 windowDurationMins 不是非负整数")]
    InvalidWindowDuration,
}

/// App Server 单个额度窗口的稳定字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    #[serde(rename = "usedPercent", alias = "used_percent")]
    pub used_percent: f32,
    #[serde(rename = "windowDurationMins", alias = "window_duration_mins")]
    pub window_duration_mins: u32,
    #[serde(default, rename = "resetsAt", alias = "resets_at", alias = "resetsAtUnix", alias = "resets_at_unix")]
    pub resets_at_unix: Option<i64>,
}

/// 只保留额度协议的两个命名窗口；其它字段由 `raw` 保留，便于未来协议演进。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitResponse {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    #[serde(skip)]
    pub raw: Option<Value>,
}

impl RateLimitResponse {
    /// 根据窗口时长产生 application 层的原子快照。
    #[must_use]
    pub fn snapshot(&self, observed_at_unix_ms: i64, revision: u64) -> RateLimitSnapshot {
        let mut five_hour = None;
        let mut weekly = None;

        // primary/secondary 的传输顺序和语义名称都不可靠，按时长分类才能兼容两种顺序。
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

/// 从 JSON 字符串解析 `result.rateLimits`、`rateLimits` 或直接窗口对象。
pub fn parse_rate_limits(input: &str) -> Result<RateLimitResponse, RateLimitError> {
    let value: Value = serde_json::from_str(input).map_err(|error| RateLimitError::InvalidJson(error.to_string()))?;
    parse_rate_limits_value(&value)
}

/// 从已解码 JSON 解析额度响应，便于事件流复用同一 Value。
pub fn parse_rate_limits_value(value: &Value) -> Result<RateLimitResponse, RateLimitError> {
    let object = find_rate_limits_object(value).ok_or(RateLimitError::MissingRateLimits)?;
    let primary = parse_window(object.get("primary"))?;
    let secondary = parse_window(object.get("secondary"))?;
    if primary.is_none() && secondary.is_none() {
        return Err(RateLimitError::MissingRateLimits);
    }
    Ok(RateLimitResponse { primary, secondary, raw: Some(Value::Object(object.clone())) })
}

/// 一步完成额度响应到 application 快照的转换。
pub fn parse_rate_limit_snapshot(
    input: &str,
    observed_at_unix_ms: i64,
    revision: u64,
) -> Result<RateLimitSnapshot, RateLimitError> {
    Ok(parse_rate_limits(input)?.snapshot(observed_at_unix_ms, revision))
}

/// `parse_rate_limit_snapshot` 的响应对象别名，名称与 RPC 读取操作对应。
pub fn parse_rate_limit_response(
    input: &str,
    observed_at_unix_ms: i64,
    revision: u64,
) -> Result<RateLimitSnapshot, RateLimitError> {
    parse_rate_limit_snapshot(input, observed_at_unix_ms, revision)
}

fn find_rate_limits_object(value: &Value) -> Option<&Map<String, Value>> {
    let object = value.as_object()?;
    if object.contains_key("primary") || object.contains_key("secondary") {
        return Some(object);
    }
    for key in ["rateLimits", "rate_limits", "result", "params"] {
        if let Some(child) = object.get(key) {
            if let Some(found) = find_rate_limits_object(child) {
                return Some(found);
            }
        }
    }
    None
}

fn parse_window(value: Option<&Value>) -> Result<Option<RateLimitWindow>, RateLimitError> {
    let Some(value) = value else { return Ok(None) };
    // sparse null 的语义是“本次没有更新”，调用方合并时会保留旧窗口。
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or(RateLimitError::InvalidWindow)?;
    let used_percent = number_f32(object.get("usedPercent").or_else(|| object.get("used_percent"))).unwrap_or(0.0);
    let duration_value = object.get("windowDurationMins").or_else(|| object.get("window_duration_mins"));
    let window_duration_mins = match duration_value {
        Some(value) => number_u32(value).ok_or(RateLimitError::InvalidWindowDuration)?,
        // 某些 updated 通知只推送 usedPercent；0 表示未知，合并时不会误判为 5h/weekly。
        None => 0,
    };
    let resets_at_unix = object
        .get("resetsAt")
        .or_else(|| object.get("resets_at"))
        .or_else(|| object.get("resetsAtUnix"))
        .or_else(|| object.get("resets_at_unix"))
        .and_then(number_i64);
    Ok(Some(RateLimitWindow { used_percent, window_duration_mins, resets_at_unix }))
}

fn number_f32(value: Option<&Value>) -> Option<f32> {
    value.and_then(Value::as_f64).map(|value| value as f32)
}

fn number_u32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok())
}

fn number_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_only_has_no_five_hour_window() {
        let response = parse_rate_limits(
            r#"{"result":{"rateLimits":{"secondary":{"usedPercent":27,"windowDurationMins":10080,"resetsAt":42}}}}"#,
        )
        .unwrap();
        let snapshot = response.snapshot(1_000, 4);
        assert!(snapshot.five_hour.is_none());
        assert_eq!(snapshot.weekly.unwrap().used_percent, 27.0);
    }

    #[test]
    fn primary_and_secondary_are_classified_by_duration_not_order() {
        let response = parse_rate_limits(
            r#"{"rateLimits":{"primary":{"usedPercent":80,"windowDurationMins":10080},"secondary":{"usedPercent":12,"windowDurationMins":300}}}"#,
        )
        .unwrap();
        let snapshot = response.snapshot(0, 0);
        assert_eq!(snapshot.five_hour.unwrap().used_percent, 12.0);
        assert_eq!(snapshot.weekly.unwrap().used_percent, 80.0);
    }

    #[test]
    fn unknown_window_is_ignored_without_becoming_weekly() {
        let response = parse_rate_limits(r#"{"primary":{"usedPercent":10,"windowDurationMins":60}}"#).unwrap();
        let snapshot = response.snapshot(0, 0);
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.weekly.is_none());
    }
}

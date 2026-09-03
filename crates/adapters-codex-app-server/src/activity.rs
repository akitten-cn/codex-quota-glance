//! App Server 事件到领域活动状态的归一化。
//!
//! 事件方法名和 status 字段在不同 Codex 版本间会有小幅变化。本模块先按更稳定
//! 的生命周期（turn/item/approval）分类，再读取可选 status；未知事件返回 None，
//! 不会把任务栏误点亮。

use codex_taskbar_domain::activity::ActivityState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一个已归一化的 App Server 活动事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub method: String,
    pub state: ActivityState,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    /// 保留完整通知，便于诊断未知字段而不泄露到日志层。
    pub raw: Value,
}

/// 无状态事件解析器；不绑定线程、不持有进程句柄。
#[derive(Debug, Default, Clone, Copy)]
pub struct EventParser;

impl EventParser {
    /// 解析一行 JSON-RPC 通知。
    #[must_use]
    pub fn parse(&self, input: &str) -> Option<ActivityEvent> {
        let value: Value = serde_json::from_str(input).ok()?;
        parse_activity_event_value(&value)
    }

    /// `parse_line` 是读取 stdin 行流的语义别名。
    #[must_use]
    pub fn parse_line(&self, input: &str) -> Option<ActivityEvent> {
        self.parse(input)
    }
}

/// 解析 JSON-RPC 通知并映射为领域活动事件。
#[must_use]
pub fn parse_activity_event(input: &str) -> Option<ActivityEvent> {
    EventParser.parse(input)
}

/// `Value` 版本的事件映射，供状态合并器避免重复 JSON 解码。
#[must_use]
pub fn parse_activity_event_value(message: &Value) -> Option<ActivityEvent> {
    let method = message.get("method")?.as_str()?.to_owned();
    let lower_method = method.to_ascii_lowercase();
    let params = message.get("params").unwrap_or(message);
    let status = find_string(params, &["status", "state", "phase", "type"]).unwrap_or_default().to_ascii_lowercase();
    let status_state = status_to_state(&status);

    let state = if is_waiting_method(&lower_method, params) {
        if is_failure_status(&status) { ActivityState::Failed } else { ActivityState::WaitingForUser }
    } else if lower_method == "thread/status/changed" || lower_method == "thread/status_changed" {
        if has_waiting_active_flag(params) {
            ActivityState::WaitingForUser
        } else {
            status_state.unwrap_or(ActivityState::Unknown)
        }
    } else if lower_method == "turn/started" || lower_method == "turn/started/changed" {
        ActivityState::Thinking
    } else if lower_method == "turn/completed" || lower_method == "turn/complete" {
        if is_failure_status(&status) { ActivityState::Failed } else { ActivityState::Completed }
    } else if lower_method == "turn/failed" {
        ActivityState::Failed
    } else if lower_method == "item/started" || lower_method == "item/started/changed" {
        item_started_state(params, status_state)
    } else if lower_method == "item/completed" || lower_method == "item/complete" {
        if is_failure_status(&status) { ActivityState::Failed } else { ActivityState::Completed }
    } else if lower_method == "item/failed" || lower_method.contains("/failed") || lower_method.ends_with("failed") {
        ActivityState::Failed
    } else {
        return None;
    };

    Some(ActivityEvent {
        method,
        state,
        thread_id: find_id(params, &["threadId", "thread_id"]),
        turn_id: find_id(params, &["turnId", "turn_id"]),
        item_id: find_id(params, &["itemId", "item_id"]),
        raw: message.clone(),
    })
}

fn has_waiting_active_flag(params: &Value) -> bool {
    params.get("status").and_then(|status| status.get("activeFlags")).and_then(Value::as_array).is_some_and(|flags| {
        flags.iter().filter_map(Value::as_str).any(|flag| {
            flag.eq_ignore_ascii_case("waitingOnApproval") || flag.eq_ignore_ascii_case("waitingOnUserInput")
        })
    })
}

/// 只返回活动状态的便捷 API，适合任务栏聚合器。
#[must_use]
pub fn activity_state_for_event(message: &Value) -> Option<ActivityState> {
    parse_activity_event_value(message).map(|event| event.state)
}

/// 兼容更短的调用方命名。
#[must_use]
pub fn map_activity_event(message: &Value) -> Option<ActivityState> {
    activity_state_for_event(message)
}

fn is_waiting_method(method: &str, params: &Value) -> bool {
    let approval = method.contains("approval") || method.contains("execapproval") || method.contains("permissions");
    let user_input = method.contains("userinput")
        || method.contains("user_input")
        || method.contains("requestuserinput")
        || method.contains("request_user_input");
    let request = method.contains("request")
        || method.contains("requested")
        || method.contains("pending")
        || method.contains("required")
        || method.contains("needs");
    let request_payload =
        find_string(params, &["kind", "type", "requestType", "request_type"]).unwrap_or_default().to_ascii_lowercase();
    let payload_is_waiting = request_payload.contains("approval")
        || request_payload.contains("userinput")
        || request_payload.contains("user_input");
    ((approval || user_input) && request) || (method == "server/request" && payload_is_waiting)
}

fn item_started_state(params: &Value, status_state: Option<ActivityState>) -> ActivityState {
    let item_type =
        find_string(params, &["itemType", "item_type", "type", "kind"]).unwrap_or_default().to_ascii_lowercase();
    if item_type.contains("approval") || item_type.contains("user_input") || item_type.contains("userinput") {
        ActivityState::WaitingForUser
    } else if item_type.contains("command")
        || item_type.contains("tool")
        || item_type.contains("shell")
        || item_type.contains("patch")
        || item_type.contains("file")
    {
        ActivityState::Executing
    } else {
        status_state.unwrap_or(ActivityState::Thinking)
    }
}

fn status_to_state(status: &str) -> Option<ActivityState> {
    if status.is_empty() {
        return None;
    }
    if is_failure_status(status) {
        return Some(ActivityState::Failed);
    }
    if status.contains("wait")
        || status.contains("approv")
        || status.contains("user_input")
        || status.contains("userinput")
        || status.contains("needs_input")
    {
        return Some(ActivityState::WaitingForUser);
    }
    if status.contains("complete") || status == "done" || status == "success" || status == "succeeded" {
        return Some(ActivityState::Completed);
    }
    if status.contains("idle") || status == "notrunning" {
        return Some(ActivityState::Idle);
    }
    if status.contains("execut")
        || status.contains("tool")
        || status.contains("command")
        || status.contains("shell")
        || status.contains("running")
    {
        return Some(ActivityState::Executing);
    }
    if status.contains("think")
        || status.contains("reason")
        || status.contains("progress")
        || status.contains("active")
        || status.contains("start")
    {
        return Some(ActivityState::Thinking);
    }
    None
}

fn is_failure_status(status: &str) -> bool {
    status.contains("fail")
        || status.contains("error")
        || status.contains("cancel")
        || status.contains("reject")
        || status.contains("denied")
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(string) = object.get(*key).and_then(Value::as_str) {
            return Some(string.to_owned());
        }
    }
    for key in ["item", "turn", "status", "request", "approval"] {
        if let Some(child) = object.get(key) {
            if let Some(found) = find_string(child, keys) {
                return Some(found);
            }
        }
    }
    None
}

fn find_id(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(id) = object.get(*key) {
            if let Some(string) = id.as_str() {
                return Some(string.to_owned());
            }
            if let Some(number) = id.as_i64() {
                return Some(number.to_string());
            }
        }
    }
    for key in ["item", "turn", "thread"] {
        if let Some(child) = object.get(key) {
            if let Some(found) = find_id(child, keys) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(json: &str) -> ActivityState {
        parse_activity_event(json).unwrap().state
    }

    #[test]
    fn maps_lifecycle_events() {
        assert_eq!(
            state(r#"{"method":"thread/status/changed","params":{"status":"active"}}"#),
            ActivityState::Thinking
        );
        assert_eq!(state(r#"{"method":"turn/started","params":{"threadId":"t"}}"#), ActivityState::Thinking);
        assert_eq!(
            state(r#"{"method":"item/started","params":{"item":{"type":"command"}}}"#),
            ActivityState::Executing
        );
        assert_eq!(state(r#"{"method":"item/completed","params":{"status":"completed"}}"#), ActivityState::Completed);
        assert_eq!(state(r#"{"method":"turn/completed","params":{"status":"failed"}}"#), ActivityState::Failed);
    }

    #[test]
    fn approval_and_user_input_wait_for_user() {
        assert_eq!(
            state(r#"{"method":"execApproval/requested","params":{"threadId":"t"}}"#),
            ActivityState::WaitingForUser
        );
        assert_eq!(
            state(r#"{"method":"requestUserInput/requested","params":{"threadId":"t"}}"#),
            ActivityState::WaitingForUser
        );
        assert_eq!(
            state(r#"{"method":"item/started","params":{"item":{"type":"approval"}}}"#),
            ActivityState::WaitingForUser
        );
        assert_eq!(
            state(
                r#"{"method":"thread/status/changed","params":{"threadId":"t","status":{"type":"active","activeFlags":["waitingOnApproval"]}}}"#
            ),
            ActivityState::WaitingForUser
        );
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert!(parse_activity_event(r#"{"method":"new/event","params":{}}"#).is_none());
    }
}

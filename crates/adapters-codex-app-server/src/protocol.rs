//! JSON-RPC 2.0 的请求和响应外壳。
//!
//! 请求构造器只生成协议消息，不负责启动进程或传输字节；这样可以在没有 Codex
//! 安装和用户数据的环境中测试协议兼容性。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// JSON-RPC 响应外壳解析错误。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("JSON-RPC 消息不是合法 JSON：{0}")]
    InvalidJson(String),
    #[error("JSON-RPC 消息的 jsonrpc 版本不是 2.0")]
    InvalidVersion,
}

/// 可直接写入 App Server stdin 的 JSON-RPC 请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    /// 创建一个带参数的 JSON-RPC 2.0 请求。
    #[must_use]
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: "2.0".to_owned(), id, method: method.into(), params }
    }

    /// `initialize` 握手请求。`clientInfo` 是 App Server 用于能力协商的稳定标识。
    #[must_use]
    pub fn initialize(id: u64, client_name: impl Into<String>, client_version: impl Into<String>) -> Self {
        Self::new(
            id,
            "initialize",
            json!({
                "clientInfo": {
                    "name": client_name.into(),
                    "version": client_version.into(),
                }
            }),
        )
    }

    /// 读取当前账户概要。
    #[must_use]
    pub fn account_read(id: u64) -> Self {
        Self::new(id, "account/read", Value::Object(Default::default()))
    }

    /// 读取账户额度；响应中的 `primary`/`secondary` 窗口由额度解析器分类。
    #[must_use]
    pub fn rate_limits_read(id: u64) -> Self {
        Self::new(id, "account/rateLimits/read", Value::Object(Default::default()))
    }

    /// 读取账户 Token 使用量。
    #[must_use]
    pub fn account_usage_read(id: u64) -> Self {
        Self::new(id, "account/usage/read", account_usage_read_params(None))
    }

    /// 读取账户 Token 使用量，并优先请求一个当前或最近活动线程的服务端估算。
    ///
    /// `threadId` 是可选参数：省略时维持旧协议的账户级活动读取；提供时由
    /// App Server 决定该线程是否存在可用 billing route，客户端不得自行估价。
    #[must_use]
    pub fn account_usage_read_for_thread(id: u64, thread_id: Option<&str>) -> Self {
        Self::new(id, "account/usage/read", account_usage_read_params(thread_id))
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// 可接收成功或失败的 JSON-RPC 响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Codex App Server 的部分版本和官方示例省略该字段；存在时必须为 2.0。
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

/// 解析一行 JSON-RPC 响应；事件通知请使用活动解析器。
pub fn parse_json_rpc_response(input: &str) -> Result<JsonRpcResponse, ProtocolError> {
    let response: JsonRpcResponse =
        serde_json::from_str(input).map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    if response.jsonrpc.as_deref().is_some_and(|version| version != "2.0") {
        return Err(ProtocolError::InvalidVersion);
    }
    Ok(response)
}

/// 与调用方常见命名对应的别名。
pub fn parse_response(input: &str) -> Result<JsonRpcResponse, ProtocolError> {
    parse_json_rpc_response(input)
}

/// `initialized` 是无 id 的通知，必须在 initialize 成功后发送。
#[must_use]
pub fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "initialized", "params": {}})
}

/// 便于调用方使用函数式 API 的 initialize 构造器。
#[must_use]
pub fn initialize_request(
    id: u64,
    client_name: impl Into<String>,
    client_version: impl Into<String>,
) -> JsonRpcRequest {
    JsonRpcRequest::initialize(id, client_name, client_version)
}

/// `initialize_request` 的简短别名。
#[must_use]
pub fn initialize(id: u64, client_name: impl Into<String>, client_version: impl Into<String>) -> JsonRpcRequest {
    initialize_request(id, client_name, client_version)
}

/// 便于调用方使用函数式 API 的账户读取构造器。
#[must_use]
pub fn account_read_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest::account_read(id)
}

/// 便于调用方使用函数式 API 的额度读取构造器。
#[must_use]
pub fn rate_limits_read_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest::rate_limits_read(id)
}

/// 带完整 RPC 方法名语义的额度读取别名。
#[must_use]
pub fn account_rate_limits_read_request(id: u64) -> JsonRpcRequest {
    rate_limits_read_request(id)
}

/// 便于调用方使用函数式 API 的账户用量读取构造器。
#[must_use]
pub fn account_usage_read_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest::account_usage_read(id)
}

/// 构造 `account/usage/read` 的可选线程参数。
///
/// 该形状对应 codex-cli 0.149.0-alpha.4.1 的
/// `NullableGetAccountTokenUsageParams` schema。空字符串不作为线程标识发送。
#[must_use]
pub fn account_usage_read_params(thread_id: Option<&str>) -> Value {
    thread_id
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
        .map_or_else(|| Value::Object(Default::default()), |thread_id| json!({"threadId": thread_id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_protocol_requests_without_io() {
        let request = initialize_request(7, "taskbar", "0.1.0");
        assert_eq!(request.method, "initialize");
        assert_eq!(request.params["clientInfo"]["name"], "taskbar");
        assert_eq!(account_read_request(8).method, "account/read");
        assert_eq!(rate_limits_read_request(9).method, "account/rateLimits/read");
        assert_eq!(account_usage_read_request(10).method, "account/usage/read");
        let threaded = JsonRpcRequest::account_usage_read_for_thread(11, Some("thread-current"));
        assert_eq!(threaded.method, "account/usage/read");
        assert_eq!(threaded.params["threadId"], "thread-current");
        assert_eq!(account_usage_read_params(Some(" ")), serde_json::json!({}));
        assert_eq!(initialized_notification()["method"], "initialized");
    }

    #[test]
    fn parses_success_and_error_responses() {
        let success = parse_json_rpc_response(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(success.result.unwrap()["ok"], true);
        let versionless = parse_response(r#"{"id":2,"result":{"ok":true}}"#).unwrap();
        assert_eq!(versionless.id, Some(serde_json::json!(2)));
        let error = parse_response(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"nope"}}"#).unwrap();
        assert_eq!(error.error.unwrap().code, -1);
    }
}

//! Codex App Server 的纯协议适配层。
//!
//! 本 crate 负责 JSON-RPC 请求/响应、额度解析、稀疏快照合并、事件归一化，及对
//! 它不启动 codex 进程；进程生命周期和其余 I/O 由上层负责。

mod activity;
pub mod locator;
mod protocol;
mod quota;
pub mod session;
mod state;
pub mod supervisor;
pub mod transport;

pub use activity::{
    ActivityEvent, EventParser, activity_state_for_event, map_activity_event, parse_activity_event,
    parse_activity_event_value,
};
pub use locator::{
    CapabilityProbe, CodexCliLocator, CodexCliLocatorInput, CodexCliSource, LocatedCodexCli, LocatorError,
    ProbeFailure, ProcessCapabilityProbe, SafeCliSummary, locate_codex_cli,
};
pub use protocol::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, ProtocolError, account_rate_limits_read_request,
    account_read_request, account_usage_read_params, account_usage_read_request, initialize, initialize_request,
    initialized_notification, parse_json_rpc_response, parse_response, rate_limits_read_request,
};
pub use quota::{
    RateLimitError, RateLimitResponse, RateLimitWindow, parse_rate_limit_response, parse_rate_limit_snapshot,
    parse_rate_limits, parse_rate_limits_value,
};
pub use session::{
    CodexSession, CodexSessionConfig, SessionFreshness, SessionUpdate, SourceHealth, start_process, start_session,
    start_session_with_factory,
};
pub use state::{
    AccountSnapshot, AccountUsageSnapshot, AppServerState, RateLimitsSnapshot, TokenUsageUpdate, merge_account,
    merge_rate_limits, merge_token_usage, replace_account, replace_account_usage, replace_rate_limits,
};
pub use supervisor::{AppServerSupervisor, ExponentialBackoff, StdioTransportConfig, SupervisorError};
pub use transport::{
    AppServerTransport, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_MAX_LINE_BYTES, DEFAULT_REQUEST_TIMEOUT, DecodedFrame,
    LineCodec, PendingResponse, TransportError, TransportEvent,
};

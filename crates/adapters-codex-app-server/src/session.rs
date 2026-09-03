//! 面向应用主循环的 Codex App Server 会话。
//!
//! [`crate::transport::AppServerTransport`] 只处理 JSON-RPC 帧；本模块在后台线程
//! 完成握手后的初始读取、周期刷新和通知合并，并发布不含原始消息日志的
//! [`SessionUpdate`]。重连工厂由 supervisor 提供，等待退避可被 `stop` 立即取消。

use crate::activity::ActivityEvent;
use crate::protocol::JsonRpcResponse;
use crate::state::AppServerState;
use crate::supervisor::{AppServerSupervisor, ExponentialBackoff, StdioTransportConfig, SupervisorError};
use crate::transport::{AppServerTransport, TransportEvent};
use codex_taskbar_application::{RateLimitSnapshot, TokenUsageSnapshot};
use codex_taskbar_domain::official::OfficialEndpointStatus;
use codex_taskbar_domain::usage::{TokenCounts, UsageSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 会话数据来源健康状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceHealth {
    /// 尚未完成 initialize/initialized 或初始快照。
    Starting,
    /// 最近一次读取或通知成功。
    Healthy,
    /// 读取失败但会话仍在运行，后续刷新可能恢复。
    Degraded,
    /// 管道已断开，正在等待 supervisor 退避重连。
    Disconnected,
    /// 已被调用方取消。
    Stopped,
}

/// 会话数据新鲜度。年龄使用单调逻辑计算，时间戳只供调用方展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionFreshness {
    /// 最近一次数据在 stale_after 内。
    Fresh,
    /// 已超过 stale_after。
    Stale,
    /// 尚未收到过完整或部分快照。
    Unknown,
}

/// 一次可直接交给应用主循环的归一化更新。
///
/// `quota` 始终是完整的 application 快照；窗口未知时对应字段为 `None`，不会
/// 把未知窗口猜成 5h 或 weekly。`account`、`usage` 和 `activity` 是截至本次
/// 更新的稀疏状态；它们不携带未经处理的服务端日志或 prompt。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionUpdate {
    pub quota: RateLimitSnapshot,
    /// 已脱敏、可直接进入应用状态的官方账户详情。
    pub official: codex_taskbar_domain::official::OfficialSnapshot,
    pub usage: Option<TokenUsageSnapshot>,
    pub activity: Option<ActivityEvent>,
    pub freshness: SessionFreshness,
    pub source_health: SourceHealth,
    /// 最近一次收到 App Server 数据的 Unix 毫秒时间戳。
    pub observed_at_unix_ms: Option<i64>,
    /// 从最近一次数据到本次发布时的年龄；未知时为 None。
    pub age: Option<Duration>,
    /// 账户切换后，要求应用层先清除上一账户范围内的缓存状态。
    ///
    /// 该信号只随账户 generation 变化后的第一份聚合快照发布一次。
    pub reset_account_scoped_state: bool,
}

/// 会话刷新和重连参数。
#[derive(Debug, Clone)]
pub struct CodexSessionConfig {
    pub refresh_interval: Duration,
    pub request_timeout: Duration,
    pub stale_after: Duration,
    pub reconnect_backoff: ExponentialBackoff,
    /// 0 表示无限重连；非 0 表示最多尝试次数。
    pub max_reconnect_attempts: u32,
}

#[derive(Debug, Clone, Copy)]
enum OfficialEndpointKind {
    Account,
    Quota,
    Usage,
}

/// 三个官方 RPC 各自维护最后成功时间。失败只降级对应区域，不能把其他区域
/// 一并标红，也不能被其他端点的成功重新标成 Live。
#[derive(Debug, Clone, Copy, Default)]
struct OfficialEndpointTracker {
    account: OfficialEndpointStatus,
    quota: OfficialEndpointStatus,
    usage: OfficialEndpointStatus,
}

impl OfficialEndpointTracker {
    fn mark_live(&mut self, kind: OfficialEndpointKind, observed_at_unix_ms: i64) {
        *self.status_mut(kind) = OfficialEndpointStatus::live(observed_at_unix_ms);
    }

    fn mark_failed(&mut self, kind: OfficialEndpointKind) {
        *self.status_mut(kind) = self.status(kind).cached();
    }

    fn mark_all_cached(&mut self) {
        self.account = self.account.cached();
        self.quota = self.quota.cached();
        self.usage = self.usage.cached();
    }

    const fn status(self, kind: OfficialEndpointKind) -> OfficialEndpointStatus {
        match kind {
            OfficialEndpointKind::Account => self.account,
            OfficialEndpointKind::Quota => self.quota,
            OfficialEndpointKind::Usage => self.usage,
        }
    }

    fn status_mut(&mut self, kind: OfficialEndpointKind) -> &mut OfficialEndpointStatus {
        match kind {
            OfficialEndpointKind::Account => &mut self.account,
            OfficialEndpointKind::Quota => &mut self.quota,
            OfficialEndpointKind::Usage => &mut self.usage,
        }
    }
}

/// 只有携带额度 patch 的通知可以刷新额度端点时间。
///
/// thread/turn/item 等活动通知虽然代表 App Server 连接仍然活跃，但不能证明
/// account/read、rateLimits/read 或 account/usage/read 的旧结果仍是实时数据。
fn apply_notification_endpoint_status(tracker: &mut OfficialEndpointTracker, method: &str, observed_at_unix_ms: i64) {
    if matches!(method, "account/rateLimits/updated" | "account/rate_limits/updated") {
        tracker.mark_live(OfficialEndpointKind::Quota, observed_at_unix_ms);
    }
}

fn notification_resets_account_generation(method: &str) -> bool {
    method == "account/updated"
}

fn stamp_token_usage_notification(state: &mut AppServerState, method: &str, observed_at_unix_ms: i64) {
    if matches!(method, "thread/tokenUsage/updated" | "thread/token_usage/updated") {
        if let Some(token_usage) = state.token_usage.as_mut() {
            token_usage.observed_at_unix_ms = Some(observed_at_unix_ms);
        }
    }
}

/// 开启新的账户数据 generation，清除所有不得跨账户继承的适配器缓存。
fn reset_account_generation(state: &mut AppServerState, endpoint_tracker: &mut OfficialEndpointTracker) {
    state.account = Default::default();
    state.rate_limits = Default::default();
    state.account_usage = None;
    state.token_usage = None;
    state.last_activity = None;
    state.thread_activities.clear();
    *endpoint_tracker = OfficialEndpointTracker::default();
}

impl Default for CodexSessionConfig {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            stale_after: Duration::from_secs(180),
            reconnect_backoff: ExponentialBackoff::default(),
            max_reconnect_attempts: 0,
        }
    }
}

/// 会话生命周期控制句柄。
pub struct CodexSession {
    cancel: Arc<Cancellation>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CodexSession").finish_non_exhaustive()
    }
}

/// 用一个已经创建的 transport 启动会话，不自动重连。
pub fn start_session(
    transport: AppServerTransport,
    events: mpsc::Receiver<TransportEvent>,
    config: CodexSessionConfig,
) -> (CodexSession, mpsc::Receiver<SessionUpdate>) {
    start_worker(transport, events, config)
}

/// 用 supervisor 工厂启动会话。工厂只在后台线程调用，断线后按退避策略重试。
pub fn start_session_with_factory<F>(
    factory: F,
    config: CodexSessionConfig,
) -> (CodexSession, mpsc::Receiver<SessionUpdate>)
where
    F: FnMut() -> Result<(AppServerTransport, mpsc::Receiver<TransportEvent>), SupervisorError> + Send + 'static,
{
    let (updates_tx, updates_rx) = mpsc::channel();
    let cancel = Arc::new(Cancellation::default());
    let worker_cancel = Arc::clone(&cancel);
    let join = thread::Builder::new()
        .name("codex-session".to_owned())
        .spawn(move || run_factory(Box::new(factory), config, worker_cancel, updates_tx))
        .expect("创建 CodexSession 线程失败");
    (CodexSession { cancel, join: Mutex::new(Some(join)) }, updates_rx)
}

/// 显式启动 `codex app-server`，但握手、刷新和重连均在后台完成。
pub fn start_process(
    process_config: StdioTransportConfig,
    config: CodexSessionConfig,
) -> (CodexSession, mpsc::Receiver<SessionUpdate>) {
    let supervisor = AppServerSupervisor;
    start_session_with_factory(move || supervisor.spawn(&process_config), config)
}

impl CodexSession {
    /// `start_session` 的关联函数形式，便于应用主循环按类型发现 API。
    pub fn start(
        transport: AppServerTransport,
        events: mpsc::Receiver<TransportEvent>,
        config: CodexSessionConfig,
    ) -> (Self, mpsc::Receiver<SessionUpdate>) {
        start_session(transport, events, config)
    }

    /// `start_session_with_factory` 的关联函数形式。
    pub fn start_with_factory<F>(factory: F, config: CodexSessionConfig) -> (Self, mpsc::Receiver<SessionUpdate>)
    where
        F: FnMut() -> Result<(AppServerTransport, mpsc::Receiver<TransportEvent>), SupervisorError> + Send + 'static,
    {
        start_session_with_factory(factory, config)
    }

    /// `start_process` 的关联函数形式。
    pub fn start_process(
        process_config: StdioTransportConfig,
        config: CodexSessionConfig,
    ) -> (Self, mpsc::Receiver<SessionUpdate>) {
        start_process(process_config, config)
    }

    /// 取消后台刷新、退避等待和重连，并请求当前 transport 关闭。
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// 等待后台线程退出；不会重新启动已停止的会话。
    pub fn join(&self) {
        if let Some(join) = self.join.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            let _ = join.join();
        }
    }

    /// 是否已收到停止请求。
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 请求立即重新读取官方账户、额度和账户级用量。
    ///
    /// 请求会合并：连续点击只会触发下一轮一次完整读取，避免详情卡片的刷新
    /// 按钮并发创建多组 JSON-RPC 请求。会话尚未 ready 时保留该请求，待连接
    /// 建立后优先执行。
    pub fn request_refresh(&self) {
        self.cancel.request_refresh();
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Drop 不等待线程，防止 UI 线程因坏掉的 reader 而阻塞；后台线程会看到
        // cancellation 并关闭当前 transport。需要同步退出时显式调用 join。
    }
}

struct Cancellation {
    cancelled: AtomicBool,
    refresh_requested: AtomicBool,
    wake: Condvar,
    lock: Mutex<()>,
}

impl Default for Cancellation {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            refresh_requested: AtomicBool::new(false),
            wake: Condvar::new(),
            lock: Mutex::new(()),
        }
    }
}

impl Cancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn take_refresh_requested(&self) -> bool {
        self.refresh_requested.swap(false, Ordering::AcqRel)
    }

    fn wait(&self, duration: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let lock = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self.wake.wait_timeout(lock, duration);
        self.is_cancelled()
    }
}

type SessionFactory =
    Box<dyn FnMut() -> Result<(AppServerTransport, mpsc::Receiver<TransportEvent>), SupervisorError> + Send>;

fn start_worker(
    transport: AppServerTransport,
    events: mpsc::Receiver<TransportEvent>,
    config: CodexSessionConfig,
) -> (CodexSession, mpsc::Receiver<SessionUpdate>) {
    let (updates_tx, updates_rx) = mpsc::channel();
    let cancel = Arc::new(Cancellation::default());
    let worker_cancel = Arc::clone(&cancel);
    let join = thread::Builder::new()
        .name("codex-session".to_owned())
        .spawn(move || run_transport(transport, events, config, None, worker_cancel, updates_tx))
        .expect("创建 CodexSession 线程失败");
    (CodexSession { cancel, join: Mutex::new(Some(join)) }, updates_rx)
}

fn run_factory(
    mut factory: SessionFactory,
    config: CodexSessionConfig,
    cancel: Arc<Cancellation>,
    updates: mpsc::Sender<SessionUpdate>,
) {
    let mut attempt: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match factory() {
            Ok((transport, events)) => {
                run_transport(
                    transport,
                    events,
                    config.clone(),
                    Some(&mut factory),
                    Arc::clone(&cancel),
                    updates.clone(),
                );
                if cancel.is_cancelled() {
                    return;
                }
                attempt = 1;
            }
            Err(_) => {
                emit_health_only(&updates, SourceHealth::Disconnected, SessionFreshness::Unknown);
                attempt = attempt.saturating_add(1);
            }
        }
        if config.max_reconnect_attempts != 0 && attempt > config.max_reconnect_attempts {
            emit_health_only(&updates, SourceHealth::Disconnected, SessionFreshness::Unknown);
            return;
        }
        if cancel.wait(config.reconnect_backoff.delay_for(attempt.saturating_sub(1))) {
            return;
        }
    }
}

fn run_transport(
    transport: AppServerTransport,
    events: mpsc::Receiver<TransportEvent>,
    config: CodexSessionConfig,
    mut factory: Option<&mut SessionFactory>,
    cancel: Arc<Cancellation>,
    updates: mpsc::Sender<SessionUpdate>,
) {
    let mut state = AppServerState::default();
    let mut endpoint_tracker = OfficialEndpointTracker::default();
    let mut health = SourceHealth::Starting;
    let mut freshness = SessionFreshness::Unknown;
    let mut last_data = None;
    let mut revision = 0;
    let transport = transport;
    let mut next_refresh = Instant::now();
    loop {
        if cancel.is_cancelled() {
            transport.close().ok();
            endpoint_tracker.mark_all_cached();
            emit_state(&updates, &state, health_stopped(&cancel), freshness, last_data, revision, endpoint_tracker);
            return;
        }
        // `RefreshRequested` 由原生宿主经运行时传到这里。不能只唤醒 Condvar：
        // transport 的事件循环并不在 Condvar 上等待，必须在这一层消费标记并
        // 执行完整聚合刷新，才能让详情卡片的“刷新”真正生效。
        if transport.is_ready() && cancel.take_refresh_requested() {
            refresh(
                &transport,
                &config,
                &mut state,
                &mut endpoint_tracker,
                &mut revision,
                &mut last_data,
                &mut health,
                &mut freshness,
                false,
                &updates,
            );
            next_refresh = Instant::now() + config.refresh_interval;
        }
        let until_refresh = next_refresh.saturating_duration_since(Instant::now());
        let wait_for_event = until_refresh.min(Duration::from_millis(100));
        match events.recv_timeout(wait_for_event) {
            Ok(TransportEvent::Ready) => {
                health = SourceHealth::Starting;
                refresh(
                    &transport,
                    &config,
                    &mut state,
                    &mut endpoint_tracker,
                    &mut revision,
                    &mut last_data,
                    &mut health,
                    &mut freshness,
                    false,
                    &updates,
                );
                next_refresh = Instant::now() + config.refresh_interval;
            }
            Ok(TransportEvent::Notification(notification)) => {
                let method = notification.get("method").and_then(Value::as_str).unwrap_or_default();
                let account_changed = notification_resets_account_generation(method);
                if account_changed {
                    // 账户切换属于新的数据 generation。先隔离旧身份、额度和账户
                    // usage，再读取完整快照，禁止发布混合账户卡片。
                    reset_account_generation(&mut state, &mut endpoint_tracker);
                }
                state.apply_notification(&notification);
                stamp_token_usage_notification(&mut state, method, now_unix_ms());
                revision = revision.saturating_add(1);
                last_data = Some(std::time::Instant::now());
                health = SourceHealth::Healthy;
                freshness = SessionFreshness::Fresh;
                apply_notification_endpoint_status(&mut endpoint_tracker, method, now_unix_ms());
                // account/updated 不携带完整身份（例如邮箱），也可能代表登录账户已切换。
                // 立即重新读取完整快照，避免把旧账户的额度或缓存继续显示给新账户。
                if account_changed && transport.is_ready() {
                    refresh(
                        &transport,
                        &config,
                        &mut state,
                        &mut endpoint_tracker,
                        &mut revision,
                        &mut last_data,
                        &mut health,
                        &mut freshness,
                        true,
                        &updates,
                    );
                    next_refresh = Instant::now() + config.refresh_interval;
                } else {
                    emit_state_with_reset(
                        &updates,
                        &state,
                        health,
                        freshness,
                        last_data,
                        revision,
                        endpoint_tracker,
                        account_changed,
                    );
                }
            }
            Ok(TransportEvent::StartupFailed(_)) | Ok(TransportEvent::Eof) | Ok(TransportEvent::IoError(_)) => {
                transport.close().ok();
                health = SourceHealth::Disconnected;
                freshness = freshness_for(last_data, config.stale_after);
                endpoint_tracker.mark_all_cached();
                emit_state(&updates, &state, health, freshness, last_data, revision, endpoint_tracker);
                if let Some(next_factory) = factory.as_deref_mut() {
                    let _ = reconnect_from_factory(
                        next_factory,
                        config,
                        cancel,
                        updates,
                        state,
                        revision,
                        endpoint_tracker,
                    );
                }
                return;
            }
            Ok(TransportEvent::Closed) => {
                health = if cancel.is_cancelled() { SourceHealth::Stopped } else { SourceHealth::Disconnected };
                freshness = freshness_for(last_data, config.stale_after);
                endpoint_tracker.mark_all_cached();
                emit_state(&updates, &state, health, freshness, last_data, revision, endpoint_tracker);
                return;
            }
            Ok(TransportEvent::ProtocolError(_)) | Ok(TransportEvent::UnexpectedResponse(_)) => {
                health = SourceHealth::Degraded;
                freshness = freshness_for(last_data, config.stale_after);
                emit_state(&updates, &state, health, freshness, last_data, revision, endpoint_tracker);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_refresh {
                    if transport.is_ready() {
                        refresh(
                            &transport,
                            &config,
                            &mut state,
                            &mut endpoint_tracker,
                            &mut revision,
                            &mut last_data,
                            &mut health,
                            &mut freshness,
                            false,
                            &updates,
                        );
                    } else {
                        freshness = freshness_for(last_data, config.stale_after);
                        emit_state(&updates, &state, health, freshness, last_data, revision, endpoint_tracker);
                    }
                    next_refresh = Instant::now() + config.refresh_interval;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                transport.close().ok();
                health = SourceHealth::Disconnected;
                freshness = freshness_for(last_data, config.stale_after);
                endpoint_tracker.mark_all_cached();
                emit_state(&updates, &state, health, freshness, last_data, revision, endpoint_tracker);
                return;
            }
        }
    }
}

fn reconnect_from_factory(
    factory: &mut SessionFactory,
    config: CodexSessionConfig,
    cancel: Arc<Cancellation>,
    updates: mpsc::Sender<SessionUpdate>,
    state: AppServerState,
    revision: u64,
    endpoint_tracker: OfficialEndpointTracker,
) -> Result<(), ()> {
    let mut attempt = 0u32;
    loop {
        if cancel.wait(config.reconnect_backoff.delay_for(attempt)) {
            return Err(());
        }
        match factory() {
            Ok((transport, events)) => {
                run_transport(transport, events, config, None, cancel, updates);
                return Ok(());
            }
            Err(_) => {
                emit_state(
                    &updates,
                    &state,
                    SourceHealth::Disconnected,
                    SessionFreshness::Stale,
                    None,
                    revision,
                    endpoint_tracker,
                );
                attempt = attempt.saturating_add(1);
                if config.max_reconnect_attempts != 0 && attempt >= config.max_reconnect_attempts {
                    return Err(());
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh(
    transport: &AppServerTransport,
    config: &CodexSessionConfig,
    state: &mut AppServerState,
    endpoint_tracker: &mut OfficialEndpointTracker,
    revision: &mut u64,
    last_data: &mut Option<std::time::Instant>,
    health: &mut SourceHealth,
    freshness: &mut SessionFreshness,
    reset_account_scoped_state: bool,
    updates: &mpsc::Sender<SessionUpdate>,
) {
    let usage_params = crate::protocol::account_usage_read_params(preferred_usage_thread_id(state).as_deref());
    let requests = [
        ("account/read", "account", OfficialEndpointKind::Account, Value::Object(Default::default())),
        ("account/rateLimits/read", "rate_limits", OfficialEndpointKind::Quota, Value::Object(Default::default())),
        ("account/usage/read", "usage", OfficialEndpointKind::Usage, usage_params),
    ];
    let mut pending = Vec::new();
    let mut failed = false;
    for (method, kind, endpoint, params) in requests {
        match transport.request(method, params) {
            Ok(response) => pending.push((kind, endpoint, response)),
            Err(_) => {
                failed = true;
                endpoint_tracker.mark_failed(endpoint);
            }
        }
    }
    let mut succeeded = false;
    for (kind, endpoint, response) in pending {
        match response.recv_timeout(config.request_timeout) {
            Ok(response) => {
                if apply_completed_response(state, endpoint_tracker, kind, endpoint, &response, now_unix_ms()) {
                    *revision = revision.saturating_add(1);
                    *last_data = Some(std::time::Instant::now());
                    succeeded = true;
                } else {
                    failed = true;
                    endpoint_tracker.mark_failed(endpoint);
                }
            }
            Err(_) => {
                failed = true;
                endpoint_tracker.mark_failed(endpoint);
            }
        }
    }
    *health = if failed { SourceHealth::Degraded } else { SourceHealth::Healthy };
    *freshness = if succeeded { SessionFreshness::Fresh } else { freshness_for(*last_data, config.stale_after) };
    // 一轮刷新只发布一个聚合快照，避免 account → quota → usage 逐个到达时
    // 短暂出现“新账户 + 旧额度”或“新额度 + 旧账户 Token”。
    emit_state_with_reset(
        updates,
        state,
        *health,
        *freshness,
        *last_data,
        *revision,
        *endpoint_tracker,
        reset_account_scoped_state,
    );
}

/// 选择 `account/usage/read` 的可选 threadId。活动线程优先于最新 Token 通知：
/// 前者表达当前用户正在看的任务，后者在没有活动事件时提供最近一次可验证线程。
fn preferred_usage_thread_id(state: &AppServerState) -> Option<String> {
    state
        .aggregated_activity(now_unix_ms())
        .and_then(|activity| activity.thread_id)
        .or_else(|| state.token_usage.as_ref().and_then(|usage| usage.thread_id.clone()))
}

/// 处理一个已经返回的 RPC：只有成功解析并实际替换了对应数据域，才把该端点标为 Live。
fn apply_completed_response(
    state: &mut AppServerState,
    endpoint_tracker: &mut OfficialEndpointTracker,
    kind: &str,
    endpoint: OfficialEndpointKind,
    response: &JsonRpcResponse,
    observed_at_unix_ms: i64,
) -> bool {
    if response.error.is_none() && response.result.is_some() && apply_response(state, kind, response) {
        endpoint_tracker.mark_live(endpoint, observed_at_unix_ms);
        true
    } else {
        endpoint_tracker.mark_failed(endpoint);
        false
    }
}

fn apply_response(state: &mut AppServerState, kind: &str, response: &JsonRpcResponse) -> bool {
    let Some(result) = &response.result else { return false };
    match kind {
        "account" => {
            let valid_account = result.as_object().is_some_and(|object| {
                object.contains_key("account")
                    || object.get("requiresOpenaiAuth").and_then(Value::as_bool).is_some()
                    || object.get("requires_openai_auth").and_then(Value::as_bool).is_some()
            });
            if !valid_account {
                return false;
            }
            crate::state::replace_account(&mut state.account, result);
            true
        }
        "rate_limits" => crate::state::replace_rate_limits(&mut state.rate_limits, result),
        // account/usage/read 是账户级聚合，不能覆盖 thread/tokenUsage/updated 的
        // 当前线程计数；两者在 UI 中分别展示。
        "usage" => {
            let has_summary =
                result.as_object().and_then(|object| object.get("summary")).and_then(Value::as_object).is_some();
            has_summary && crate::state::replace_account_usage(&mut state.account_usage, result)
        }
        _ => false,
    }
}

fn emit_state(
    updates: &mpsc::Sender<SessionUpdate>,
    state: &AppServerState,
    health: SourceHealth,
    freshness: SessionFreshness,
    last_data: Option<std::time::Instant>,
    revision: u64,
    endpoint_tracker: OfficialEndpointTracker,
) {
    emit_state_with_reset(updates, state, health, freshness, last_data, revision, endpoint_tracker, false);
}

#[allow(clippy::too_many_arguments)]
fn emit_state_with_reset(
    updates: &mpsc::Sender<SessionUpdate>,
    state: &AppServerState,
    health: SourceHealth,
    freshness: SessionFreshness,
    last_data: Option<std::time::Instant>,
    revision: u64,
    endpoint_tracker: OfficialEndpointTracker,
    reset_account_scoped_state: bool,
) {
    let now = now_unix_ms();
    let age = last_data.map(|instant| instant.elapsed());
    let update = SessionUpdate {
        quota: state.rate_limits.snapshot(now, revision),
        official: state.official_snapshot_with_status(
            endpoint_tracker.account,
            endpoint_tracker.quota,
            endpoint_tracker.usage,
        ),
        usage: usage_snapshot(state, now),
        activity: state.aggregated_activity(now),
        freshness,
        source_health: health,
        observed_at_unix_ms: last_data.map(|_| now),
        age,
        reset_account_scoped_state,
    };
    let _ = updates.send(update);
}

fn usage_snapshot(state: &AppServerState, observed_at_unix_ms: i64) -> Option<TokenUsageSnapshot> {
    let usage = state.token_usage.as_ref()?;
    Some(TokenUsageSnapshot {
        current_thread: Some(TokenCounts {
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            cache_write_input: usage.cache_write_input_tokens,
            output: usage.output_tokens,
            reasoning_output: usage.reasoning_output_tokens,
            total: usage.total_tokens,
        }),
        last_turn: usage.last.clone(),
        model_context_window: usage.model_context_window,
        today: None,
        observed_at_unix_ms: usage.observed_at_unix_ms.unwrap_or(observed_at_unix_ms),
        source: UsageSource::AppServer,
    })
}

fn emit_health_only(updates: &mpsc::Sender<SessionUpdate>, health: SourceHealth, freshness: SessionFreshness) {
    let state = AppServerState::default();
    emit_state(updates, &state, health, freshness, None, 0, OfficialEndpointTracker::default());
}

fn freshness_for(last_data: Option<std::time::Instant>, stale_after: Duration) -> SessionFreshness {
    match last_data {
        None => SessionFreshness::Unknown,
        Some(instant) if instant.elapsed() <= stale_after => SessionFreshness::Fresh,
        Some(_) => SessionFreshness::Stale,
    }
}

fn health_stopped(cancel: &Cancellation) -> SourceHealth {
    if cancel.is_cancelled() { SourceHealth::Stopped } else { SourceHealth::Disconnected }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{LineCodec, spawn_with_io};
    use codex_taskbar_domain::official::OfficialFreshness;
    use std::collections::VecDeque;
    use std::io;

    #[derive(Clone, Default)]
    struct FakeIo {
        input: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl FakeIo {
        fn push(&self, text: &str) {
            self.input.0.lock().unwrap().extend(text.as_bytes());
            self.input.1.notify_all();
        }
    }

    struct FakeReader(FakeIo);
    impl std::io::Read for FakeReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let mut input = self.0.input.0.lock().unwrap();
            while input.is_empty() {
                input = self.0.input.1.wait(input).unwrap();
            }
            let count = bytes.len().min(input.len());
            for byte in &mut bytes[..count] {
                *byte = input.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    struct FakeWriter(FakeIo);
    impl std::io::Write for FakeWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.output.lock().unwrap().extend_from_slice(bytes);
            for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
                let value: Value = serde_json::from_slice(line).unwrap();
                let Some(id) = value.get("id").and_then(Value::as_u64) else { continue };
                match id {
                    1 => self.0.push("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n"),
                    2 => self.0.push("{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"account\":{\"type\":\"chatgpt\",\"email\":\"safe@example.com\",\"planType\":\"plus\"},\"requiresOpenaiAuth\":true}}\n"),
                    3 => self.0.push("{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":20,\"windowDurationMins\":300}}}}\n"),
                    4 => self.0.push("{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"summary\":{\"lifetimeTokens\":10},\"dailyUsageBuckets\":[]}}\n"),
                    _ => {}
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fake_server_produces_snapshot_without_logging_raw_input() {
        let io = FakeIo::default();
        let (transport, events) = spawn_with_io(
            FakeReader(io.clone()),
            FakeWriter(io),
            "test",
            "0.1",
            LineCodec::default(),
            Duration::from_millis(100),
            Duration::from_secs(1),
        );
        let (session, updates) = start_session(
            transport,
            events,
            CodexSessionConfig {
                refresh_interval: Duration::from_secs(10),
                request_timeout: Duration::from_secs(1),
                ..CodexSessionConfig::default()
            },
        );
        let mut got_account = false;
        let mut got_quota = false;
        let mut got_usage = false;
        for _ in 0..20 {
            if let Ok(update) = updates.recv_timeout(Duration::from_millis(200)) {
                got_account |=
                    update.official.account.as_ref().and_then(|account| account.masked_identifier.as_deref()).is_some();
                got_quota |= update.quota.five_hour.is_some();
                got_usage |= update.official.account_usage.as_ref().and_then(|usage| usage.lifetime_tokens).is_some();
                if got_account && got_quota && got_usage {
                    break;
                }
            }
        }
        assert!(got_account && got_quota && got_usage);
        session.stop();
        session.join();
    }

    #[test]
    fn usage_snapshot_keeps_last_turn_cache_write_and_context_window_separate() {
        let state = AppServerState {
            token_usage: Some(crate::state::TokenUsageUpdate {
                input_tokens: Some(30),
                cached_input_tokens: Some(20),
                cache_write_input_tokens: Some(8),
                output_tokens: Some(10),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(40),
                last: Some(TokenCounts {
                    input: Some(3),
                    cached_input: Some(2),
                    cache_write_input: Some(1),
                    output: Some(1),
                    reasoning_output: Some(1),
                    total: Some(4),
                }),
                model_context_window: Some(200_000),
                observed_at_unix_ms: Some(123),
                ..crate::state::TokenUsageUpdate::default()
            }),
            ..AppServerState::default()
        };

        let snapshot = usage_snapshot(&state, 123).expect("thread usage snapshot");
        let total = snapshot.current_thread.expect("cumulative total");
        let last = snapshot.last_turn.expect("last turn");
        assert_eq!(total.cache_write_input, Some(8));
        assert_eq!(total.total, Some(40));
        assert_eq!(last.cache_write_input, Some(1));
        assert_eq!(last.total, Some(4));
        assert_eq!(snapshot.model_context_window, Some(200_000));
        assert_eq!(snapshot.observed_at_unix_ms, 123);
    }

    #[test]
    fn endpoint_failure_only_downgrades_its_own_data_domain() {
        let mut tracker = OfficialEndpointTracker::default();
        tracker.mark_live(OfficialEndpointKind::Account, 100);
        tracker.mark_live(OfficialEndpointKind::Quota, 200);
        tracker.mark_live(OfficialEndpointKind::Usage, 300);

        tracker.mark_failed(OfficialEndpointKind::Quota);

        assert_eq!(tracker.account.freshness, OfficialFreshness::Live);
        assert_eq!(tracker.account.observed_at_unix_ms, Some(100));
        assert_eq!(tracker.quota.freshness, OfficialFreshness::Cached);
        assert_eq!(tracker.quota.observed_at_unix_ms, Some(200));
        assert_eq!(tracker.usage.freshness, OfficialFreshness::Live);
        assert_eq!(tracker.usage.observed_at_unix_ms, Some(300));
    }

    #[test]
    fn thread_and_unknown_notifications_do_not_refresh_official_endpoints() {
        let mut tracker = OfficialEndpointTracker::default();
        tracker.mark_live(OfficialEndpointKind::Account, 100);
        tracker.mark_live(OfficialEndpointKind::Quota, 200);
        tracker.mark_live(OfficialEndpointKind::Usage, 300);
        let before = tracker;

        for method in ["thread/tokenUsage/updated", "turn/started", "future/unknown", "account/updated"] {
            apply_notification_endpoint_status(&mut tracker, method, 999);
            assert_eq!(tracker.account, before.account);
            assert_eq!(tracker.quota, before.quota);
            assert_eq!(tracker.usage, before.usage);
        }

        apply_notification_endpoint_status(&mut tracker, "account/rateLimits/updated", 999);
        assert_eq!(tracker.account, before.account);
        assert_eq!(tracker.quota, OfficialEndpointStatus::live(999));
        assert_eq!(tracker.usage, before.usage);
    }

    #[test]
    fn only_account_updated_resets_account_generation() {
        assert!(notification_resets_account_generation("account/updated"));
        for method in ["thread/tokenUsage/updated", "turn/started", "account/rateLimits/updated", "future/unknown"] {
            assert!(!notification_resets_account_generation(method));
        }
    }

    #[test]
    fn activity_notifications_do_not_refresh_thread_token_observation_time() {
        let mut state = AppServerState {
            token_usage: Some(crate::state::TokenUsageUpdate {
                total_tokens: Some(42),
                observed_at_unix_ms: Some(100),
                ..crate::state::TokenUsageUpdate::default()
            }),
            ..AppServerState::default()
        };

        stamp_token_usage_notification(&mut state, "turn/started", 200);
        assert_eq!(state.token_usage.as_ref().and_then(|usage| usage.observed_at_unix_ms), Some(100));

        stamp_token_usage_notification(&mut state, "thread/tokenUsage/updated", 300);
        assert_eq!(state.token_usage.as_ref().and_then(|usage| usage.observed_at_unix_ms), Some(300));
    }

    #[test]
    fn explicit_refresh_request_is_consumed_once() {
        let cancellation = Cancellation::default();

        cancellation.request_refresh();
        assert!(cancellation.take_refresh_requested());
        assert!(!cancellation.take_refresh_requested());
    }

    #[test]
    fn account_usage_refresh_prefers_active_thread_then_latest_token_thread() {
        let mut state = AppServerState {
            token_usage: Some(crate::state::TokenUsageUpdate {
                thread_id: Some("token-thread".to_owned()),
                ..crate::state::TokenUsageUpdate::default()
            }),
            ..AppServerState::default()
        };
        assert_eq!(preferred_usage_thread_id(&state).as_deref(), Some("token-thread"));

        state.apply_notification_at(
            &serde_json::json!({"method":"turn/started","params":{"threadId":"active-thread"}}),
            now_unix_ms(),
        );
        assert_eq!(preferred_usage_thread_id(&state).as_deref(), Some("active-thread"));
    }

    #[test]
    fn invalid_success_payload_is_cached_instead_of_marked_live() {
        let mut state = AppServerState::default();
        assert!(crate::state::replace_rate_limits(
            &mut state.rate_limits,
            &serde_json::json!({
                "rateLimits": {"primary": {"usedPercent": 20, "windowDurationMins": 300}}
            })
        ));
        assert!(crate::state::replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({"summary": {"lifetimeTokens": 10}, "dailyUsageBuckets": []})
        ));
        let mut tracker = OfficialEndpointTracker::default();
        tracker.mark_live(OfficialEndpointKind::Quota, 100);
        tracker.mark_live(OfficialEndpointKind::Usage, 100);

        let invalid_quota = JsonRpcResponse {
            jsonrpc: Some("2.0".to_owned()),
            id: Some(serde_json::json!(1)),
            result: Some(serde_json::json!({"unexpected": true})),
            error: None,
        };
        assert!(!apply_completed_response(
            &mut state,
            &mut tracker,
            "rate_limits",
            OfficialEndpointKind::Quota,
            &invalid_quota,
            200,
        ));
        assert_eq!(tracker.quota.freshness, OfficialFreshness::Cached);
        assert_eq!(tracker.quota.observed_at_unix_ms, Some(100));
        assert_eq!(state.rate_limits.primary.as_ref().map(|window| window.used_percent), Some(20.0));

        let invalid_usage = JsonRpcResponse {
            jsonrpc: Some("2.0".to_owned()),
            id: Some(serde_json::json!(2)),
            result: Some(serde_json::json!({"summary": null, "dailyUsageBuckets": []})),
            error: None,
        };
        assert!(!apply_completed_response(
            &mut state,
            &mut tracker,
            "usage",
            OfficialEndpointKind::Usage,
            &invalid_usage,
            200,
        ));
        assert_eq!(tracker.usage.freshness, OfficialFreshness::Cached);
        assert_eq!(tracker.usage.observed_at_unix_ms, Some(100));
        assert_eq!(
            state
                .account_usage
                .as_ref()
                .and_then(|usage| usage.raw.get("summary"))
                .and_then(|summary| summary.get("lifetimeTokens"))
                .and_then(Value::as_u64),
            Some(10)
        );
    }

    #[test]
    fn account_generation_reset_clears_old_account_quota_usage_and_thread_tokens() {
        let mut state = AppServerState {
            token_usage: Some(crate::state::TokenUsageUpdate {
                total_tokens: Some(99),
                ..crate::state::TokenUsageUpdate::default()
            }),
            ..AppServerState::default()
        };
        crate::state::replace_account(
            &mut state.account,
            &serde_json::json!({"account": {"type": "chatgpt", "email": "old@example.com"}}),
        );
        assert!(crate::state::replace_rate_limits(
            &mut state.rate_limits,
            &serde_json::json!({"rateLimits": {"primary": {"usedPercent": 20, "windowDurationMins": 300}}})
        ));
        assert!(crate::state::replace_account_usage(
            &mut state.account_usage,
            &serde_json::json!({"summary": {"lifetimeTokens": 10}, "dailyUsageBuckets": []})
        ));
        let mut tracker = OfficialEndpointTracker::default();
        tracker.mark_live(OfficialEndpointKind::Account, 100);
        tracker.mark_live(OfficialEndpointKind::Quota, 100);
        tracker.mark_live(OfficialEndpointKind::Usage, 100);

        reset_account_generation(&mut state, &mut tracker);

        assert_eq!(state.account, Default::default());
        assert_eq!(state.rate_limits, Default::default());
        assert!(state.account_usage.is_none());
        assert!(state.token_usage.is_none());
        assert_eq!(tracker.account, OfficialEndpointStatus::default());
        assert_eq!(tracker.quota, OfficialEndpointStatus::default());
        assert_eq!(tracker.usage, OfficialEndpointStatus::default());
    }

    #[test]
    fn account_reset_signal_is_only_present_on_the_marked_snapshot() {
        let (updates, received) = mpsc::channel();
        let state = AppServerState::default();
        emit_state_with_reset(
            &updates,
            &state,
            SourceHealth::Healthy,
            SessionFreshness::Fresh,
            None,
            1,
            OfficialEndpointTracker::default(),
            true,
        );
        emit_state(
            &updates,
            &state,
            SourceHealth::Healthy,
            SessionFreshness::Fresh,
            None,
            2,
            OfficialEndpointTracker::default(),
        );

        assert!(received.recv().expect("reset snapshot").reset_account_scoped_state);
        assert!(!received.recv().expect("ordinary snapshot").reset_account_scoped_state);
    }
}

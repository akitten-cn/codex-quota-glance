//! Codex App Server 的 stdio JSON-RPC 传输层。
//!
//! 传输层只使用标准库线程和 channel，不依赖异步运行时。读取、写入和请求
//! 匹配都在后台线程中完成，因此调用方（包括 UI 线程）只需要把请求放入
//! channel 即可返回。进程的启动配置位于 [`crate::supervisor`]。

use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, ProtocolError, initialized_notification};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

/// 默认单行上限，避免服务端异常输出导致无限内存增长。
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;
/// 默认普通请求超时。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 默认握手超时。
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 传输层错误。错误内容只包含协议或 I/O 状态，不包含环境变量、路径中的
/// 用户数据，也不会自动记录服务端输出。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("App Server stdio 已关闭")]
    Closed,
    #[error("App Server stdio 已到达 EOF")]
    Eof,
    #[error("请求 {id} 超时")]
    Timeout { id: u64 },
    #[error("stdio I/O 失败：{0}")]
    Io(String),
    #[error("JSON-RPC 协议错误：{0}")]
    Protocol(ProtocolError),
    #[error("服务端返回 JSON-RPC 错误 {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("响应消息缺少有效的数值 id")]
    MissingResponseId,
    #[error("JSON-RPC 行超过上限 {limit} 字节")]
    LineTooLong { limit: usize },
    #[error("JSON-RPC 行不是合法 UTF-8")]
    InvalidUtf8,
    #[error("JSON-RPC 消息格式无效：{0}")]
    InvalidMessage(String),
    #[error("请求 id 已耗尽")]
    IdExhausted,
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<ProtocolError> for TransportError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// 传输层收到的事件。匹配到请求的响应会交给 [`PendingResponse`]，不重复
/// 投递到事件 channel；未匹配响应会作为事件保留，便于诊断协议错位。
#[derive(Debug, Clone, PartialEq)]
pub enum TransportEvent {
    /// 服务端无 id 的 JSON-RPC 通知。
    Notification(Value),
    /// 没有等待者的响应（通常意味着服务端或客户端 id 已错位）。
    UnexpectedResponse(JsonRpcResponse),
    /// 一行消息无法解析；读取线程会继续处理后续行。
    ProtocolError(TransportError),
    /// stdin/stdout 管道正常读到 EOF。
    Eof,
    /// 握手完成，普通请求现在可以发送。
    Ready,
    /// transport 主动关闭。
    Closed,
    /// 写入线程或读取线程遇到不可恢复的 I/O 错误。
    IoError(TransportError),
    /// 初始化握手失败。
    StartupFailed(TransportError),
}

/// 受限的一行 JSON 编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCodec {
    max_line_bytes: usize,
}

impl Default for LineCodec {
    fn default() -> Self {
        Self { max_line_bytes: DEFAULT_MAX_LINE_BYTES }
    }
}

impl LineCodec {
    /// 创建带指定最大行长度的编解码器；0 表示不接受任何非空行。
    #[must_use]
    pub const fn new(max_line_bytes: usize) -> Self {
        Self { max_line_bytes }
    }

    /// 返回单行最大字节数。
    #[must_use]
    pub const fn max_line_bytes(self) -> usize {
        self.max_line_bytes
    }

    /// 将 JSON 值编码成带换行的 NDJSON 帧。
    pub fn encode_value(&self, value: &Value) -> Result<Vec<u8>, TransportError> {
        let mut bytes = serde_json::to_vec(value).map_err(|error| TransportError::InvalidMessage(error.to_string()))?;
        if bytes.len() > self.max_line_bytes {
            return Err(TransportError::LineTooLong { limit: self.max_line_bytes });
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// 将请求编码成带换行的 NDJSON 帧。
    pub fn encode_request(&self, request: &JsonRpcRequest) -> Result<Vec<u8>, TransportError> {
        self.encode_value(
            &serde_json::to_value(request).map_err(|error| TransportError::InvalidMessage(error.to_string()))?,
        )
    }

    /// 从 BufRead 读取一帧。超长帧会被完整丢弃到换行处，之后仍可继续读取。
    pub fn read_frame<R: BufRead>(&self, reader: &mut R) -> Result<Option<Vec<u8>>, TransportError> {
        let mut frame = Vec::with_capacity(self.max_line_bytes.min(4096));
        let mut too_long = false;
        loop {
            let available = reader.fill_buf().map_err(TransportError::from)?;
            if available.is_empty() {
                if frame.is_empty() {
                    return Ok(None);
                }
                if too_long {
                    return Err(TransportError::LineTooLong { limit: self.max_line_bytes });
                }
                return Ok(Some(frame));
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if !too_long {
                let content_len = newline.unwrap_or(available.len());
                let room = self.max_line_bytes.saturating_sub(frame.len());
                let copy_len = content_len.min(room);
                frame.extend_from_slice(&available[..copy_len]);
                if content_len > room {
                    too_long = true;
                }
            }
            reader.consume(consumed);
            if newline.is_some() {
                if too_long {
                    return Err(TransportError::LineTooLong { limit: self.max_line_bytes });
                }
                return Ok(Some(frame));
            }
        }
    }

    /// 将一帧解析成通知或响应。空白行视为无消息。
    pub fn decode_frame(&self, frame: &[u8]) -> Result<Option<DecodedFrame>, TransportError> {
        let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
        if frame.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let text = std::str::from_utf8(frame).map_err(|_| TransportError::InvalidUtf8)?;
        let value: Value = serde_json::from_str(text)
            .map_err(|error| TransportError::Protocol(ProtocolError::InvalidJson(error.to_string())))?;
        let object =
            value.as_object().ok_or_else(|| TransportError::InvalidMessage("JSON-RPC 消息必须是对象".to_owned()))?;
        if object.get("jsonrpc").is_some_and(|version| version.as_str() != Some("2.0")) {
            return Err(TransportError::Protocol(ProtocolError::InvalidVersion));
        }

        if object.get("method").is_some() && object.get("id").is_none() {
            if object.get("method").and_then(Value::as_str).is_none() {
                return Err(TransportError::InvalidMessage("通知 method 必须是字符串".to_owned()));
            }
            return Ok(Some(DecodedFrame::Notification(value)));
        }

        let response: JsonRpcResponse =
            serde_json::from_value(value).map_err(|error| TransportError::InvalidMessage(error.to_string()))?;
        if response.id.as_ref().and_then(Value::as_u64).is_none() {
            return Err(TransportError::MissingResponseId);
        }
        Ok(Some(DecodedFrame::Response(response)))
    }
}

/// 编解码后的消息分类。
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedFrame {
    Notification(Value),
    Response(JsonRpcResponse),
}

/// 一次请求的异步响应句柄。
#[derive(Debug)]
pub struct PendingResponse {
    id: u64,
    receiver: mpsc::Receiver<Result<JsonRpcResponse, TransportError>>,
}

impl PendingResponse {
    /// 返回由 transport 分配的递增请求 id。
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// 阻塞等待响应；调用方应在后台线程使用，UI 线程可使用 `try_recv`。
    pub fn recv(self) -> Result<JsonRpcResponse, TransportError> {
        self.receiver.recv().unwrap_or(Err(TransportError::Closed))
    }

    /// 在指定时限内等待响应。
    pub fn recv_timeout(&self, timeout: Duration) -> Result<JsonRpcResponse, TransportError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(TransportError::Timeout { id: self.id }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::Closed),
        }
    }

    /// 非阻塞取响应。
    pub fn try_recv(&self) -> Result<Option<Result<JsonRpcResponse, TransportError>>, TransportError> {
        match self.receiver.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(TransportError::Closed),
        }
    }
}

/// 发往写入线程的消息。
enum Outbound {
    Bytes(Vec<u8>),
}

enum Command {
    Request { request: JsonRpcRequest, response: mpsc::Sender<Result<JsonRpcResponse, TransportError>> },
    Close,
}

struct PendingEntry {
    sender: mpsc::Sender<Result<JsonRpcResponse, TransportError>>,
    deadline: Instant,
}

struct Shared {
    alive: AtomicBool,
    ready: AtomicBool,
    handshake_registered: AtomicBool,
    closed_event_sent: AtomicBool,
    pending: Mutex<HashMap<u64, PendingEntry>>,
    events: mpsc::Sender<TransportEvent>,
}

impl Shared {
    fn terminate(&self, error: TransportError) -> bool {
        if !self.alive.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.ready.store(false, Ordering::Release);
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_, entry) in pending.drain() {
            let _ = entry.sender.send(Err(error.clone()));
        }
        true
    }

    fn emit_closed(&self) {
        if !self.closed_event_sent.swap(true, Ordering::AcqRel) {
            let _ = self.events.send(TransportEvent::Closed);
        }
    }

    fn insert_pending(
        &self,
        id: u64,
        sender: mpsc::Sender<Result<JsonRpcResponse, TransportError>>,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.contains_key(&id) {
            return Err(TransportError::InvalidMessage(format!("请求 id {id} 重复")));
        }
        pending.insert(id, PendingEntry { sender, deadline });
        Ok(())
    }

    fn fail_pending(&self, id: u64, error: TransportError) {
        if let Some(entry) = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id) {
            let _ = entry.sender.send(Err(error));
        }
    }
}

/// 无需 async runtime 的 Codex App Server stdio transport。
pub struct AppServerTransport {
    command_tx: mpsc::Sender<Command>,
    events_rx: Option<mpsc::Receiver<TransportEvent>>,
    next_id: AtomicU64,
    shared: Arc<Shared>,
    codec: LineCodec,
}

impl std::fmt::Debug for AppServerTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerTransport")
            .field("ready", &self.shared.ready.load(Ordering::Acquire))
            .field("alive", &self.shared.alive.load(Ordering::Acquire))
            .field("max_line_bytes", &self.codec.max_line_bytes)
            .finish_non_exhaustive()
    }
}

impl AppServerTransport {
    /// 使用已准备好的 stdin/stdout 流启动 transport。该构造器不会启动任何外部
    /// 进程，适合单元测试以及由上层自行管理的子进程。
    pub fn spawn_with_io<R, W>(
        reader: R,
        writer: W,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
        codec: LineCodec,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> (Self, mpsc::Receiver<TransportEvent>)
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel();
        let (outbound_tx, outbound_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            ready: AtomicBool::new(false),
            handshake_registered: AtomicBool::new(false),
            closed_event_sent: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            events: events_tx,
        });

        let writer_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("codex-app-server-writer".to_owned())
            .spawn(move || writer_loop(writer, outbound_rx, writer_shared))
            .expect("创建 Codex App Server writer 线程失败");

        let supervisor_shared = Arc::clone(&shared);
        let supervisor_codec = codec;
        let name = client_name.into();
        let version = client_version.into();
        let supervisor_outbound = outbound_tx.clone();
        thread::Builder::new()
            .name("codex-app-server-supervisor".to_owned())
            .spawn(move || {
                supervisor_loop(
                    command_rx,
                    supervisor_outbound,
                    supervisor_shared,
                    supervisor_codec,
                    name,
                    version,
                    request_timeout,
                    handshake_timeout,
                )
            })
            .expect("创建 Codex App Server supervisor 线程失败");

        // 先让 supervisor 放入 initialize 的 pending，再启动 reader，避免一个
        // 很快 EOF 的内存流在握手注册前抢先消费响应。
        let reader_shared = Arc::clone(&shared);
        let reader_codec = codec;
        thread::Builder::new()
            .name("codex-app-server-reader".to_owned())
            .spawn(move || reader_loop(reader, reader_codec, reader_shared))
            .expect("创建 Codex App Server reader 线程失败");

        // initialize 固定占用 id=1，普通请求必须从 2 开始，避免与仍在途的握手响应冲突。
        let transport = Self { command_tx, events_rx: None, next_id: AtomicU64::new(2), shared, codec };
        (transport, events_rx)
    }

    /// 取出唯一的事件接收端。更常用的形式是使用 [`Self::spawn_with_io`] 返回值
    /// 中的第二项；此方法保留给需要把 transport 句柄单独传递的调用方。
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<TransportEvent>> {
        self.events_rx.take()
    }

    /// 当前是否已完成 initialize/initialized 握手。
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    /// 当前管道是否仍可使用。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    /// 发送一个方法请求；id 由 transport 递增分配，传入 request 的 id 会被覆盖。
    pub fn request(&self, method: impl Into<String>, params: Value) -> Result<PendingResponse, TransportError> {
        let id = self.next_request_id()?;
        let request = JsonRpcRequest::new(id, method, params);
        self.send_request_with_id(request, id)
    }

    /// 发送已有协议请求；transport 仍会分配新 id，以保证全局递增且不冲突。
    pub fn send_request(&self, mut request: JsonRpcRequest) -> Result<PendingResponse, TransportError> {
        let id = self.next_request_id()?;
        request.id = id;
        self.send_request_with_id(request, id)
    }

    /// `request` 的语义别名，便于调用方按 RPC 术语使用。
    pub fn send(&self, request: JsonRpcRequest) -> Result<PendingResponse, TransportError> {
        self.send_request(request)
    }

    /// 请求关闭；操作本身不等待子线程，适合 UI 线程。
    pub fn close(&self) -> Result<(), TransportError> {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.command_tx.send(Command::Close).is_err() {
            self.shared.terminate(TransportError::Closed);
            self.shared.emit_closed();
            return Err(TransportError::Closed);
        }
        // 先行发布关闭状态，supervisor 随后处理命令时会通过原子标志去重。
        self.shared.emit_closed();
        Ok(())
    }

    fn next_request_id(&self) -> Result<u64, TransportError> {
        let mut current = self.next_id.load(Ordering::Relaxed);
        loop {
            if current == u64::MAX {
                return Err(TransportError::IdExhausted);
            }
            match self.next_id.compare_exchange_weak(current, current + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Ok(current),
                Err(observed) => current = observed,
            }
        }
    }

    fn send_request_with_id(&self, request: JsonRpcRequest, id: u64) -> Result<PendingResponse, TransportError> {
        if !self.is_alive() {
            return Err(TransportError::Closed);
        }
        // 先检查大小，使过大的请求在调用端立即失败，不进入无界 command queue。
        self.codec.encode_request(&request)?;
        let (sender, receiver) = mpsc::channel();
        self.command_tx.send(Command::Request { request, response: sender }).map_err(|_| TransportError::Closed)?;
        Ok(PendingResponse { id, receiver })
    }
}

/// 直接返回 transport 与事件接收端的便捷函数。
pub fn spawn_with_io<R, W>(
    reader: R,
    writer: W,
    client_name: impl Into<String>,
    client_version: impl Into<String>,
    codec: LineCodec,
    request_timeout: Duration,
    handshake_timeout: Duration,
) -> (AppServerTransport, mpsc::Receiver<TransportEvent>)
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    AppServerTransport::spawn_with_io(
        reader,
        writer,
        client_name,
        client_version,
        codec,
        request_timeout,
        handshake_timeout,
    )
}

/// 读线程内部执行的消息循环。
fn reader_loop<R: Read + Send + 'static>(reader: R, codec: LineCodec, shared: Arc<Shared>) {
    // supervisor 先登记 initialize pending，避免极短输入流在登记前丢失响应。
    while !shared.handshake_registered.load(Ordering::Acquire) {
        if !shared.alive.load(Ordering::Acquire) {
            return;
        }
        thread::yield_now();
    }
    let mut reader = BufReader::new(reader);
    loop {
        let frame = match codec.read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if shared.terminate(TransportError::Eof) {
                    let _ = shared.events.send(TransportEvent::Eof);
                }
                return;
            }
            Err(error @ TransportError::Io(_)) => {
                if shared.terminate(error.clone()) {
                    let _ = shared.events.send(TransportEvent::IoError(error));
                }
                return;
            }
            Err(error) => {
                let _ = shared.events.send(TransportEvent::ProtocolError(error));
                continue;
            }
        };
        let decoded = match codec.decode_frame(&frame) {
            Ok(decoded) => decoded,
            Err(error) => {
                let _ = shared.events.send(TransportEvent::ProtocolError(error));
                continue;
            }
        };
        match decoded {
            None => {}
            Some(DecodedFrame::Notification(notification)) => {
                let _ = shared.events.send(TransportEvent::Notification(notification));
            }
            Some(DecodedFrame::Response(response)) => {
                let Some(id) = response.id.as_ref().and_then(Value::as_u64) else {
                    let _ = shared.events.send(TransportEvent::ProtocolError(TransportError::MissingResponseId));
                    continue;
                };
                let pending = shared.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id);
                if let Some(entry) = pending {
                    let result = response.error.as_ref().map_or(Ok(response.clone()), |error| Err(rpc_error(error)));
                    let _ = entry.sender.send(result);
                } else {
                    let _ = shared.events.send(TransportEvent::UnexpectedResponse(response));
                }
            }
        }
    }
}

fn writer_loop<W: Write + Send + 'static>(mut writer: W, receiver: mpsc::Receiver<Outbound>, shared: Arc<Shared>) {
    while let Ok(Outbound::Bytes(bytes)) = receiver.recv() {
        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            let error = TransportError::Io(error.to_string());
            if shared.terminate(error.clone()) {
                let _ = shared.events.send(TransportEvent::IoError(error));
            }
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervisor_loop(
    receiver: mpsc::Receiver<Command>,
    outbound: mpsc::Sender<Outbound>,
    shared: Arc<Shared>,
    codec: LineCodec,
    client_name: String,
    client_version: String,
    request_timeout: Duration,
    handshake_timeout: Duration,
) {
    let timer_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("codex-app-server-timeouts".to_owned())
        .spawn(move || timeout_loop(timer_shared, request_timeout))
        .expect("创建 Codex App Server timeout 线程失败");

    let initialize_id = 1;
    let (initialize_sender, initialize_receiver) = mpsc::channel();
    if shared.insert_pending(initialize_id, initialize_sender, deadline_from_now(handshake_timeout)).is_err() {
        let _ = shared
            .events
            .send(TransportEvent::StartupFailed(TransportError::InvalidMessage("initialize id 冲突".to_owned())));
        shared.terminate(TransportError::Closed);
        return;
    }
    shared.handshake_registered.store(true, Ordering::Release);
    let initialize = JsonRpcRequest::initialize(initialize_id, client_name, client_version);
    let initialize_bytes = match codec.encode_request(&initialize) {
        Ok(bytes) => bytes,
        Err(error) => {
            shared.fail_pending(initialize_id, error.clone());
            let _ = shared.events.send(TransportEvent::StartupFailed(error.clone()));
            shared.terminate(error);
            return;
        }
    };
    if outbound.send(Outbound::Bytes(initialize_bytes)).is_err() {
        let error = TransportError::Closed;
        shared.fail_pending(initialize_id, error.clone());
        let _ = shared.events.send(TransportEvent::StartupFailed(error.clone()));
        shared.terminate(error);
        return;
    }

    let initialize_result = match initialize_receiver.recv_timeout(handshake_timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(TransportError::Timeout { id: initialize_id }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::Eof),
    };
    if let Err(error) = initialize_result {
        shared.fail_pending(initialize_id, error.clone());
        let _ = shared.events.send(TransportEvent::StartupFailed(error.clone()));
        shared.terminate(error);
        drain_commands(&receiver, TransportError::Closed);
        return;
    }
    let initialized = match codec.encode_value(&initialized_notification()) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = shared.events.send(TransportEvent::StartupFailed(error.clone()));
            shared.terminate(error);
            drain_commands(&receiver, TransportError::Closed);
            return;
        }
    };
    if outbound.send(Outbound::Bytes(initialized)).is_err() {
        let error = TransportError::Closed;
        let _ = shared.events.send(TransportEvent::StartupFailed(error.clone()));
        shared.terminate(error);
        drain_commands(&receiver, TransportError::Closed);
        return;
    }
    shared.ready.store(true, Ordering::Release);
    let _ = shared.events.send(TransportEvent::Ready);

    loop {
        if !shared.alive.load(Ordering::Acquire) {
            drain_commands(&receiver, TransportError::Closed);
            return;
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Command::Close) => {
                shared.terminate(TransportError::Closed);
                shared.emit_closed();
                return;
            }
            Ok(Command::Request { request, response }) => {
                if !shared.alive.load(Ordering::Acquire) {
                    let _ = response.send(Err(TransportError::Closed));
                    continue;
                }
                let id = request.id;
                if let Err(error) = shared.insert_pending(id, response, deadline_from_now(request_timeout)) {
                    let _ = shared.events.send(TransportEvent::ProtocolError(error));
                    continue;
                }
                match codec.encode_request(&request) {
                    Ok(bytes) => {
                        if outbound.send(Outbound::Bytes(bytes)).is_err() {
                            let error = TransportError::Closed;
                            shared.fail_pending(id, error.clone());
                            shared.terminate(error);
                            return;
                        }
                    }
                    Err(error) => {
                        shared.fail_pending(id, error.clone());
                        let _ = shared.events.send(TransportEvent::ProtocolError(error));
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                shared.terminate(TransportError::Closed);
                shared.emit_closed();
                return;
            }
        }
    }
}

fn timeout_loop(shared: Arc<Shared>, timeout: Duration) {
    // 默认 30 秒超时时只需每 250ms 检查一次，避免空闲时每秒唤醒 40 次。
    let interval = timeout.min(Duration::from_millis(250)).max(Duration::from_millis(1));
    while shared.alive.load(Ordering::Acquire) {
        thread::sleep(interval);
        let now = Instant::now();
        let expired = {
            let mut pending = shared.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let ids =
                pending.iter().filter_map(|(id, entry)| (entry.deadline <= now).then_some(*id)).collect::<Vec<_>>();
            ids.into_iter().filter_map(|id| pending.remove(&id).map(|entry| (id, entry))).collect::<Vec<_>>()
        };
        for (id, entry) in expired {
            let _ = entry.sender.send(Err(TransportError::Timeout { id }));
        }
    }
}

fn deadline_from_now(timeout: Duration) -> Instant {
    Instant::now().checked_add(timeout).unwrap_or_else(Instant::now)
}

fn rpc_error(error: &JsonRpcError) -> TransportError {
    TransportError::Rpc { code: error.code, message: error.message.clone() }
}

fn drain_commands(receiver: &mpsc::Receiver<Command>, error: TransportError) {
    while let Ok(command) = receiver.try_recv() {
        match command {
            Command::Request { response, .. } => {
                let _ = response.send(Err(error.clone()));
            }
            Command::Close => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::{Arc, atomic::AtomicBool};

    #[derive(Clone, Default)]
    struct ScriptState {
        bytes: Arc<(Mutex<VecDeque<u8>>, std::sync::Condvar)>,
        closed: Arc<AtomicBool>,
    }

    impl ScriptState {
        fn push(&self, text: &str) {
            let (queue, wake) = &*self.bytes;
            queue.lock().unwrap().extend(text.as_bytes());
            wake.notify_all();
        }

        fn close(&self) {
            self.closed.store(true, Ordering::Release);
            self.bytes.1.notify_all();
        }
    }

    struct ScriptReader {
        state: ScriptState,
    }

    impl Read for ScriptReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            let (queue, wake) = &*self.state.bytes;
            let mut queue = queue.lock().unwrap();
            while queue.is_empty() && !self.state.closed.load(Ordering::Acquire) {
                queue = wake.wait(queue).unwrap();
            }
            if queue.is_empty() {
                return Ok(0);
            }
            let count = target.len().min(queue.len());
            for slot in &mut target[..count] {
                *slot = queue.pop_front().expect("队列在锁内不应为空");
            }
            Ok(count)
        }
    }

    struct ScriptWriter {
        state: ScriptState,
        buffer: Vec<u8>,
        queued_second_request: bool,
    }

    impl Write for ScriptWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.extend_from_slice(bytes);
            while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line = self.buffer.drain(..=index).collect::<Vec<_>>();
                let value: Value = serde_json::from_slice(&line[..line.len() - 1]).expect("测试请求应为 JSON");
                let Some(id) = value["id"].as_u64() else {
                    // initialize 成功后的 initialized 是合法无 id 通知。
                    continue;
                };
                let method = value["method"].as_str().unwrap_or_default();
                match id {
                    1 => self.state.push(
                        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{}}\n",
                    ),
                    2 if method == "slow" => {}
                    2 => self.queued_second_request = true,
                    3 => {
                        // 故意先发 id=3 再发 id=2，验证 pending map 不依赖到达顺序。
                        self.state.push("{\"id\":3,\"result\":{\"value\":\"b\"}}\n");
                        if self.queued_second_request {
                            self.state.push("{\"id\":2,\"result\":{\"value\":\"a\"}}\n");
                        }
                    }
                    _ => {}
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scripted_transport() -> (AppServerTransport, mpsc::Receiver<TransportEvent>, ScriptState) {
        let state = ScriptState::default();
        let result = spawn_with_io(
            ScriptReader { state: state.clone() },
            ScriptWriter { state: state.clone(), buffer: Vec::new(), queued_second_request: false },
            "test",
            "0.1",
            LineCodec::default(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        (result.0, result.1, state)
    }

    fn wait_ready(events: &mpsc::Receiver<TransportEvent>) {
        for _ in 0..20 {
            if matches!(events.recv_timeout(Duration::from_millis(100)), Ok(TransportEvent::Ready)) {
                return;
            }
        }
        panic!("未收到 ready");
    }

    #[test]
    fn line_codec_rejects_long_line_and_invalid_json() {
        let codec = LineCodec::new(8);
        let mut reader = Cursor::new(b"123456789\n{bad}\n");
        assert_eq!(codec.read_frame(&mut reader), Err(TransportError::LineTooLong { limit: 8 }));
        let frame = codec.read_frame(&mut reader).unwrap().unwrap();
        assert!(matches!(codec.decode_frame(&frame), Err(TransportError::Protocol(ProtocolError::InvalidJson(_)))));
    }

    #[test]
    fn handshake_notifications_and_eof_are_reported() {
        let (transport, events, state) = scripted_transport();
        let mut saw_ready = false;
        let mut saw_notification = false;
        for _ in 0..20 {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(TransportEvent::Ready) => saw_ready = true,
                Ok(TransportEvent::Notification(_)) => saw_notification = true,
                Ok(_) | Err(_) => {}
            }
            if saw_ready && saw_notification {
                break;
            }
        }
        assert!(saw_ready, "未收到 ready");
        assert!(saw_notification, "未收到握手期间的通知");
        assert!(transport.is_ready());
        state.close();
        assert!(matches!(events.recv_timeout(Duration::from_secs(1)), Ok(TransportEvent::Eof)));
    }

    #[test]
    fn responses_match_ids_even_when_out_of_order() {
        let (transport, events, _state) = scripted_transport();
        wait_ready(&events);
        let first = transport.request("one", serde_json::json!({})).unwrap();
        let second = transport.request("two", serde_json::json!({})).unwrap();
        assert_eq!(first.id(), 2);
        assert_eq!(second.id(), 3);
        assert_eq!(first.recv().unwrap().result.unwrap()["value"], "a");
        assert_eq!(second.recv().unwrap().result.unwrap()["value"], "b");
    }

    #[test]
    fn request_timeout_is_reported_without_blocking_submission() {
        let state = ScriptState::default();
        let (transport, events) = spawn_with_io(
            ScriptReader { state: state.clone() },
            ScriptWriter { state, buffer: Vec::new(), queued_second_request: false },
            "test",
            "0.1",
            LineCodec::default(),
            Duration::from_millis(20),
            Duration::from_secs(1),
        );
        wait_ready(&events);
        let response = transport.request("slow", Value::Null).unwrap();
        assert!(matches!(response.recv_timeout(Duration::from_secs(1)), Err(TransportError::Timeout { id: 2 })));
    }

    #[test]
    fn back_to_back_close_is_safe() {
        let (transport, events, _state) = scripted_transport();
        wait_ready(&events);
        transport.close().unwrap();
        // 握手通知或 reader EOF 可能已在 Closed 前进入同一个有序队列；关闭契约只
        // 保证最终发布一次 Closed，不保证它是调用 close 后收到的第一个事件。
        let mut saw_closed = false;
        for _ in 0..20 {
            if matches!(events.recv_timeout(Duration::from_millis(100)), Ok(TransportEvent::Closed)) {
                saw_closed = true;
                break;
            }
        }
        assert!(saw_closed, "未收到 Closed");
    }
}

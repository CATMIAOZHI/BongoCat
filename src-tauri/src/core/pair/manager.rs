//! PairManager：连接生命周期、状态机、重连、发送队列与事件广播。
//!
//! 整个应用只有这一个网络连接：所有 WebView 共享同一份 managed state，前端只观察状态。
//!
//! 事件通过 [`PairEventSink`] 发出，而不是直接依赖 `AppHandle`，这样状态机可以在
//! 测试里用真实中继跑完整流程（见 e2e.rs）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager as _, Runtime};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::client::{self, PairFailure, PairSocket};
use super::crypto::{self, PairCipher};
use super::protocol::{
    AppEnvelope, FrameHeader, FrameKind, InputStats, MAX_BINARY_FRAME_SIZE, PROTOCOL_VERSION,
    PetSnapshot, PresencePayload, PresenceState, RecentMessageIds, ServerFrame, message_type,
};
use super::secret;

pub const EVENT_CONNECTION_CHANGED: &str = "pair-connection-changed";
pub const EVENT_PEER_CHANGED: &str = "pair-peer-changed";
pub const EVENT_PRESENCE: &str = "pair-presence";
pub const EVENT_PET_STATE: &str = "pair-pet-state";
pub const EVENT_STATS: &str = "pair-stats";
pub const EVENT_MESSAGE: &str = "pair-message";
pub const EVENT_ERROR: &str = "pair-error";

const RELIABLE_QUEUE_LIMIT: usize = 512;
const RECENT_MESSAGE_LIMIT: usize = 256;
pub const HEARTBEAT_ENV: &str = "BONGO_PAIR_HEARTBEAT_SECS";
const DEFAULT_HEARTBEAT_SECS: u64 = 60;
const BACKOFF_STEPS_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// 建立连接的时限。没有它时，被黑洞掉的目标会让 task 卡在 OS 层的 SYN 重试里，
/// 用户看到「正在连接」但既不能断开也不会重试。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 单次写入的时限。对端不读数据时 `send` 会一直等待；有了它，卡住的 socket 会在
/// 十几秒内被判定为断开并进入重连，disconnect 与心跳也都还能继续工作。
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairConnectionState {
    /// 配对功能未启用（由前端偏好页决定，见 docs/pair-plan.md 的状态机）
    #[allow(dead_code)]
    Disabled,
    Disconnected,
    Connecting,
    ConnectedPeerOffline,
    Connected,
    Reconnecting,
    /// 需要人工处理的错误（例如 Pair Secret 与中继不一致）
    #[allow(dead_code)]
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairStatus {
    pub state: PairConnectionState,
    pub peer_online: bool,
    pub peer_name: Option<String>,
    pub remote_presence: Option<PresenceState>,
    pub remote_stats: Option<InputStats>,
    pub device_id: String,
    pub relay_url: Option<String>,
    pub last_error: Option<String>,
}

/// 事件出口：真实运行时是 Tauri 的 `AppHandle`，测试里是记录器
pub trait PairEventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

pub struct AppEventSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> AppEventSink<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }
}

impl<R: Runtime> PairEventSink for AppEventSink<R> {
    fn emit(&self, event: &str, payload: Value) {
        let _ = self.app.emit(event, payload);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceRecord {
    #[serde(rename = "deviceId")]
    device_id: String,
}

#[derive(Clone)]
struct SessionConfig {
    relay_url: String,
    auth_token: String,
    root_key: [u8; 32],
    device_id: String,
}

enum Command {
    Send {
        kind: FrameKind,
        envelope: AppEnvelope,
    },
    /// 请连接任务取走「最新一帧可覆盖状态」并立即发送
    FlushReplaceable,
    Disconnect,
}

/// 可覆盖状态（宠物快照、统计）在 manager 这一层的暂存区。
///
/// 放在这里而不是放进命令通道，是为了让「latest wins」在入队时就成立：3Hz 的宠物
/// 快照如果逐帧塞进 channel，socket 卡住时这些已经过期的帧会白白堆积。
#[derive(Default)]
struct PendingReplaceable {
    /// key 是 `FrameKind` 的字节值，因此每种可覆盖类型各自保留最新一帧，互不顶掉
    entries: HashMap<u8, (FrameKind, AppEnvelope)>,
    /// 已经排了一次 `FlushReplaceable`，在它被取走之前不必再排
    queued: bool,
}

pub struct PairManager {
    status: Mutex<PairStatus>,
    sender: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    pending: Mutex<PendingReplaceable>,
    generation: AtomicU64,
    envelope_seq: AtomicU64,
    sink: Arc<dyn PairEventSink>,
}

impl PairManager {
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn new(device_id: String, sink: Arc<dyn PairEventSink>) -> Self {
        Self {
            status: Mutex::new(PairStatus {
                state: PairConnectionState::Disconnected,
                peer_online: false,
                peer_name: None,
                remote_presence: None,
                remote_stats: None,
                device_id,
                relay_url: None,
                last_error: None,
            }),
            sender: Mutex::new(None),
            pending: Mutex::new(PendingReplaceable::default()),
            generation: AtomicU64::new(0),
            envelope_seq: AtomicU64::new(0),
            sink,
        }
    }

    pub fn status(&self) -> PairStatus {
        Self::lock(&self.status).clone()
    }

    pub fn device_id(&self) -> String {
        Self::lock(&self.status).device_id.clone()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn next_envelope_seq(&self) -> u64 {
        self.envelope_seq.fetch_add(1, Ordering::SeqCst)
    }

    /// 让当前连接任务失效并请它退出（不阻塞）
    fn cancel_current(&self) {
        let sender = Self::lock(&self.sender).take();

        // 断线或断开后不应该再补发上一段会话的活动快照；`queued` 必须一起复位，
        // 否则新会话的第一次 send_replaceable 会以为已经排过 flush 而永远不发送
        {
            let mut pending = Self::lock(&self.pending);

            pending.entries.clear();
            pending.queued = false;
        }

        self.generation.fetch_add(1, Ordering::SeqCst);

        if let Some(sender) = sender {
            let _ = sender.send(Command::Disconnect);
        }
    }

    pub fn start(
        self: &Arc<Self>,
        relay_url: &str,
        secret_text: Option<&str>,
    ) -> Result<(), String> {
        let trimmed = relay_url.trim();

        if trimmed.is_empty() {
            return Err("请先填写 Relay URL".into());
        }

        let secret_text = match secret_text {
            Some(secret) => secret.to_string(),
            None => secret::load_secret()?.ok_or_else(|| "还没有配置 Pair Secret".to_string())?,
        };
        let secret_bytes = crypto::decode_pair_secret(&secret_text)?;

        let config = SessionConfig {
            relay_url: trimmed.to_string(),
            auth_token: crypto::derive_auth_token(&secret_bytes),
            root_key: crypto::derive_root_key(&secret_bytes),
            device_id: self.device_id(),
        };

        self.cancel_current();

        let (sender, receiver) = mpsc::unbounded_channel();

        *Self::lock(&self.sender) = Some(sender);

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let manager = Arc::clone(self);

        self.publish(generation, |status| {
            status.state = PairConnectionState::Connecting;
            status.relay_url = Some(config.relay_url.clone());
            status.last_error = None;
        });

        tauri::async_runtime::spawn(async move {
            run_session(manager, generation, config, receiver).await;
        });

        Ok(())
    }

    pub fn disconnect(self: &Arc<Self>) {
        self.cancel_current();

        let generation = self.generation();

        self.publish(generation, |status| {
            status.state = PairConnectionState::Disconnected;
            status.peer_online = false;
            status.peer_name = None;
            status.remote_presence = None;
            status.remote_stats = None;
        });
    }

    pub fn send(&self, kind: FrameKind, message_type: &str, payload: Value) -> Result<(), String> {
        self.enqueue(kind, message_type, payload)
    }

    /// 可覆盖的实时状态（宠物快照、统计）：拥塞时新数据直接覆盖旧数据
    pub fn send_replaceable(
        &self,
        kind: FrameKind,
        message_type: &str,
        payload: Value,
    ) -> Result<(), String> {
        let sender = self.sender()?;
        let envelope = AppEnvelope::new(message_type, self.next_envelope_seq(), payload);

        let should_wake = {
            let mut pending = Self::lock(&self.pending);

            pending.entries.insert(kind.as_byte(), (kind, envelope));

            if pending.queued {
                false
            } else {
                pending.queued = true;

                true
            }
        };

        if !should_wake {
            return Ok(());
        }

        if sender.send(Command::FlushReplaceable).is_err() {
            // 连接任务已经结束：撤回这次排队的标记，避免下一段会话误以为已经有 flush 在路上
            let mut pending = Self::lock(&self.pending);

            pending.queued = false;
            pending.entries.remove(&kind.as_byte());

            return Err("连接任务已结束".to_string());
        }

        Ok(())
    }

    fn sender(&self) -> Result<mpsc::UnboundedSender<Command>, String> {
        Self::lock(&self.sender)
            .clone()
            .ok_or_else(|| "当前没有连接".to_string())
    }

    /// 取走所有待发送的可覆盖状态（每次 flush 只取一次，取走后由连接任务负责送达）
    fn take_pending_replaceable(&self) -> Vec<(FrameKind, AppEnvelope)> {
        let mut pending = Self::lock(&self.pending);

        pending.queued = false;

        pending.entries.drain().map(|(_, item)| item).collect()
    }

    fn enqueue(
        &self,
        kind: FrameKind,
        message_type: &str,
        payload: Value,
    ) -> Result<(), String> {
        let sender = self.sender()?;

        let envelope = AppEnvelope::new(message_type, self.next_envelope_seq(), payload);

        sender
            .send(Command::Send { kind, envelope })
            .map_err(|_| "连接任务已结束".to_string())
    }

    /// 只在 generation 仍然有效时更新状态并广播
    fn publish(&self, generation: u64, mutate: impl FnOnce(&mut PairStatus)) {
        if generation != self.generation() {
            return;
        }

        let status = {
            let mut status = Self::lock(&self.status);

            mutate(&mut status);
            status.clone()
        };

        if let Ok(payload) = serde_json::to_value(status) {
            self.sink.emit(EVENT_CONNECTION_CHANGED, payload);
        }
    }

    /// 只在 generation 仍然有效、且文案确实发生变化时更新并广播
    fn emit_error(&self, generation: u64, message: String) {
        if generation != self.generation() {
            return;
        }

        // 长时间连不上时同一条错误会被反复触发；同文案直接跳过，避免事件与日志无上限增长
        let status = {
            let mut status = Self::lock(&self.status);

            if status.last_error.as_deref() == Some(message.as_str()) {
                return;
            }

            // 除了单独的错误事件，也写进 status.lastError，让前端打开偏好页时能直接看到原因
            status.last_error = Some(message.clone());

            status.clone()
        };

        if let Ok(payload) = serde_json::to_value(status) {
            self.sink.emit(EVENT_CONNECTION_CHANGED, payload);
        }

        self.sink.emit(EVENT_ERROR, json!({ "message": message }));
    }

    /// 进入 `Error`（需要人工处理）状态，并停下连接任务
    fn fail_hard(self: &Arc<Self>, generation: u64, message: String) {
        if generation != self.generation() {
            return;
        }

        // 状态与原因一次写完并广播，避免前端先看到 Error 再看到原因
        self.publish(generation, |status| {
            status.state = PairConnectionState::Error;
            status.peer_online = false;
            status.peer_name = None;
            status.remote_presence = None;
            status.remote_stats = None;
            status.last_error = Some(message.clone());
        });

        self.sink.emit(EVENT_ERROR, json!({ "message": message }));

        // 再让当前任务失效并清掉 sender：这样界面上的「立即连接」能重新开始，
        // 而仍在旧任务里的发送会得到「当前没有连接」，不会静默成功
        self.cancel_current();
    }
}

/// 读取（首次运行时生成）设备 id。它不是账号，只用于区分两台设备。
pub fn load_or_create_device_id<R: Runtime>(app: &AppHandle<R>) -> Result<String, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|err| format!("无法定位配置目录: {err}"))?
        .join("pair");

    std::fs::create_dir_all(&directory).map_err(|err| format!("创建配置目录失败: {err}"))?;

    let path = directory.join("device.json");

    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(record) = serde_json::from_str::<DeviceRecord>(&text)
        && is_valid_device_id(&record.device_id)
    {
        return Ok(record.device_id);
    }

    let device_id = uuid::Uuid::new_v4().to_string();
    let record = DeviceRecord {
        device_id: device_id.clone(),
    };
    let encoded =
        serde_json::to_string_pretty(&record).map_err(|err| format!("序列化失败: {err}"))?;

    std::fs::write(&path, encoded).map_err(|err| format!("写入设备 id 失败: {err}"))?;

    Ok(device_id)
}

pub fn is_valid_device_id(device_id: &str) -> bool {
    !device_id.is_empty()
        && device_id.len() <= 64
        && device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

struct SessionState {
    cipher: PairCipher,
    reliable: VecDeque<Vec<u8>>,
    /// 每种可覆盖类型各自最多留一帧（`BTreeMap` 同时保证发送顺序稳定）
    replaceable: BTreeMap<u8, Vec<u8>>,
    frame_seq: u32,
    recent: RecentMessageIds,
}

impl SessionState {
    fn new(root_key: &[u8; 32]) -> Self {
        Self {
            cipher: PairCipher::new(root_key),
            reliable: VecDeque::new(),
            replaceable: BTreeMap::new(),
            frame_seq: 0,
            recent: RecentMessageIds::new(RECENT_MESSAGE_LIMIT),
        }
    }

    fn encode(&mut self, kind: FrameKind, envelope: &AppEnvelope) -> Result<Vec<u8>, String> {
        let plaintext = envelope.to_bytes()?;
        let header = FrameHeader::new(kind, self.frame_seq);

        self.frame_seq = self.frame_seq.wrapping_add(1);

        let frame = self.cipher.seal(&header, &plaintext)?;

        // 出站同样要挡住超大帧：中继会对 >1 MiB 的帧 close 1009，而失败回滚会把
        // 这一帧放回队头，形成「重连 → 再发 → 再被关」的死循环
        if frame.len() > MAX_BINARY_FRAME_SIZE {
            return Err("待发送的帧超过中继允许的大小".to_string());
        }

        Ok(frame)
    }

    /// 返回 `true` 表示可靠队列已满、挤掉了最旧的一帧（调用方需要告知用户）
    fn queue(
        &mut self,
        kind: FrameKind,
        envelope: &AppEnvelope,
        replaceable: bool,
    ) -> Result<bool, String> {
        let frame = self.encode(kind, envelope)?;

        if replaceable {
            // 实时状态 latest wins：每类只留最新一帧，绝不无限堆积
            self.replaceable.insert(kind.as_byte(), frame);

            return Ok(false);
        }

        let dropped = if self.reliable.len() >= RELIABLE_QUEUE_LIMIT {
            self.reliable.pop_front();

            true
        } else {
            false
        };

        self.reliable.push_back(frame);

        Ok(dropped)
    }
}

enum Outcome {
    Stopped,
    Lost(PairFailure),
}

struct Backoff {
    attempt: u32,
}

impl Backoff {
    fn new() -> Self {
        Self { attempt: 0 }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }

    fn next_delay(&mut self) -> Duration {
        let index = (self.attempt as usize).min(BACKOFF_STEPS_SECS.len() - 1);
        let base = BACKOFF_STEPS_SECS[index] as f64;

        self.attempt = self.attempt.saturating_add(1);

        let jitter = 0.8 + rand::random::<f64>() * 0.4;

        Duration::from_millis((base * 1000.0 * jitter) as u64)
    }
}

fn heartbeat_interval() -> Duration {
    // 只为测试提供缩短间隔的能力，正常运行是 60 秒
    let seconds = std::env::var(HEARTBEAT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_HEARTBEAT_SECS);

    Duration::from_secs(seconds)
}

async fn run_session(
    manager: Arc<PairManager>,
    generation: u64,
    config: SessionConfig,
    mut receiver: mpsc::UnboundedReceiver<Command>,
) {
    let mut state = SessionState::new(&config.root_key);
    let mut backoff = Backoff::new();

    loop {
        if generation != manager.generation() {
            return;
        }

        manager.publish(generation, |status| {
            status.state = PairConnectionState::Connecting;
        });

        let attempt = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(&config.relay_url, &config.auth_token, &config.device_id),
        )
        .await;

        let failure = match attempt {
            Ok(Ok(socket)) => {
                backoff.reset();

                manager.publish(generation, |status| {
                    status.state = PairConnectionState::ConnectedPeerOffline;
                    status.peer_online = false;
                    status.last_error = None;
                });

                match live(&manager, generation, &mut state, socket, &mut receiver).await {
                    Outcome::Stopped => break,
                    Outcome::Lost(failure) => failure,
                }
            }
            Ok(Err(failure)) => failure,
            Err(_) => PairFailure {
                message: "连接中继超时".to_string(),
                fatal: false,
            },
        };

        if failure.fatal {
            // 重试不会好的错误：停在 Error 等用户改配置或点「立即连接」，
            // 否则填错 Pair Secret 会每 30 秒无意义地重连一次
            manager.fail_hard(generation, failure.message);

            return;
        }

        manager.emit_error(generation, failure.message);

        if generation != manager.generation() {
            return;
        }

        manager.publish(generation, |status| {
            status.state = PairConnectionState::Reconnecting;
            status.peer_online = false;
        });

        // 退避期间仍然接收命令：排队，或在用户手动断开时立即退出
        let deadline = tokio::time::Instant::now() + backoff.next_delay();

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                command = receiver.recv() => match command {
                    None | Some(Command::Disconnect) => return,
                    Some(Command::Send { kind, envelope }) => {
                        match state.queue(kind, &envelope, false) {
                            Ok(true) => manager.emit_error(
                                generation,
                                "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                            ),
                            Ok(false) => {}
                            Err(error) => manager.emit_error(generation, error),
                        }
                    }
                    Some(Command::FlushReplaceable) => {
                        for (kind, envelope) in manager.take_pending_replaceable() {
                            if let Err(error) = state.queue(kind, &envelope, true) {
                                manager.emit_error(generation, error);
                            }
                        }
                    }
                },
            }
        }
    }

    manager.publish(generation, |status| {
        status.state = PairConnectionState::Disconnected;
        status.peer_online = false;
    });
}

async fn live(
    manager: &Arc<PairManager>,
    generation: u64,
    state: &mut SessionState,
    socket: PairSocket,
    receiver: &mut mpsc::UnboundedReceiver<Command>,
) -> Outcome {
    let (mut sink, mut stream) = socket.split();

    if let Err(error) = flush(&mut sink, state).await {
        return Outcome::Lost(PairFailure {
            message: error,
            fatal: false,
        });
    }

    let heartbeat = heartbeat_interval();
    let mut ticker = tokio::time::interval(heartbeat);

    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut awaiting_pong = false;
    let mut last_inbound = tokio::time::Instant::now();

    loop {
        tokio::select! {
            command = receiver.recv() => match command {
                None | Some(Command::Disconnect) => {
                    let _ = tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Close(None))).await;

                    return Outcome::Stopped;
                }
                Some(Command::Send { kind, envelope }) => {
                    match state.queue(kind, &envelope, false) {
                        Err(error) => manager.emit_error(generation, error),
                        Ok(dropped) => {
                            if dropped {
                                manager.emit_error(
                                    generation,
                                    "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                                );
                            }

                            if let Err(error) = flush(&mut sink, state).await {
                                return Outcome::Lost(PairFailure { message: error, fatal: false });
                            }
                        }
                    }
                }
                Some(Command::FlushReplaceable) => {
                    for (kind, envelope) in manager.take_pending_replaceable() {
                        if let Err(error) = state.queue(kind, &envelope, true) {
                            manager.emit_error(generation, error);
                        }
                    }

                    if let Err(error) = flush(&mut sink, state).await {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }
                }
            },
            incoming = stream.next() => {
                let Some(incoming) = incoming else {
                    return Outcome::Lost(describe_close(None));
                };

                last_inbound = tokio::time::Instant::now();
                awaiting_pong = false;

                match incoming {
                    Err(error) => return Outcome::Lost(PairFailure {
                        message: format!("连接错误: {error}"),
                        fatal: false,
                    }),
                    Ok(Message::Binary(bytes)) => {
                        if bytes.len() > MAX_BINARY_FRAME_SIZE {
                            return Outcome::Lost(PairFailure {
                                message: "收到超过上限的帧".into(),
                                fatal: false,
                            });
                        }

                        match handle_binary(manager, generation, state, &bytes) {
                            Err(error) => manager.emit_error(generation, error),
                            Ok(Some((kind, reply))) => {
                                match state.queue(kind, &reply, false) {
                                    Err(error) => manager.emit_error(generation, error),
                                    Ok(dropped) => {
                                        if dropped {
                                            manager.emit_error(
                                                generation,
                                                "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                                            );
                                        }

                                        if let Err(error) = flush(&mut sink, state).await {
                                            return Outcome::Lost(PairFailure {
                                                message: error,
                                                fatal: false,
                                            });
                                        }
                                    }
                                }
                            }
                            Ok(None) => {}
                        }
                    }
                    Ok(Message::Text(text)) => {
                        if let Err(error) = handle_server_frame(manager, generation, text.as_str()) {
                            return Outcome::Lost(error);
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        let code = frame.map(|frame| frame.code.into());

                        return Outcome::Lost(describe_close(code));
                    }
                    Ok(_) => {}
                }
            },
            _ = ticker.tick() => {
                if awaiting_pong && last_inbound.elapsed() >= heartbeat * 2 {
                    return Outcome::Lost(PairFailure {
                        message: "心跳超时".into(),
                        fatal: false,
                    });
                }

                awaiting_pong = true;

                if let Err(error) = send_frame(&mut sink, Message::Ping(Vec::new().into())).await {
                    return Outcome::Lost(PairFailure {
                        message: error,
                        fatal: false,
                    });
                }
            },
        }
    }
}

/// 中继主动关闭时，把关闭码翻译成人能看懂的原因（取值见 server-cloudflare/src/protocol.ts）
fn describe_close(code: Option<u16>) -> PairFailure {
    let message = match code {
        Some(4002) => "这条连接被同一台设备的新连接顶替".to_string(),
        Some(4003) => "配对已满：对端已经在另一个位置连上了".to_string(),
        Some(4004) => "旧连接因长时间没有活动被顶替".to_string(),
        Some(1008) => "中继判定帧格式或发送频率异常".to_string(),
        Some(1009) => "帧超过中继允许的大小".to_string(),
        Some(1011) => "中继内部错误".to_string(),
        Some(code) => format!("中继关闭了连接（{code}）"),
        None => "中继关闭了连接".to_string(),
    };

    // 只有「配额位已被占满」这一类重试也不会好：要等另一台设备断开或由用户处理
    PairFailure {
        message,
        fatal: code == Some(4003),
    }
}

async fn flush<S>(sink: &mut S, state: &mut SessionState) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    while let Some(frame) = state.reliable.pop_front() {
        if let Err(error) = send_frame(sink, Message::Binary(frame.clone().into())).await {
            state.reliable.push_front(frame);

            return Err(error);
        }
    }

    while let Some((key, frame)) = state.replaceable.pop_first() {
        if let Err(error) = send_frame(sink, Message::Binary(frame.clone().into())).await {
            state.replaceable.insert(key, frame);

            return Err(error);
        }
    }

    Ok(())
}

/// 带超时的写入。对端不读数据时 `send` 会无限等待；超时后由调用方把连接判为断开。
async fn send_frame<S>(sink: &mut S, message: Message) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    match tokio::time::timeout(SEND_TIMEOUT, sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("发送失败: {error}")),
        Err(_) => Err("发送超时".to_string()),
    }
}

type Reply = Option<(FrameKind, AppEnvelope)>;

fn handle_binary(
    manager: &Arc<PairManager>,
    generation: u64,
    state: &mut SessionState,
    bytes: &[u8],
) -> Result<Reply, String> {
    let (header, plaintext) = state.cipher.open(bytes)?;

    if header.flags != 0 {
        return Err("收到不支持的帧标志".into());
    }

    let envelope = AppEnvelope::from_bytes(&plaintext)?;

    if envelope.v != PROTOCOL_VERSION {
        return Err(format!("不支持的消息版本: {}", envelope.v));
    }

    // 重连或重发可能带来重复消息
    if !state.recent.insert(&envelope.id) {
        return Ok(None);
    }

    match envelope.message_type.as_str() {
        message_type::PONG => Ok(None),
        message_type::PING => Ok(Some((
            FrameKind::Ping,
            AppEnvelope::new(
                message_type::PONG,
                manager.next_envelope_seq(),
                envelope.payload.clone(),
            ),
        ))),
        message_type::PRESENCE => {
            // 载荷不合法就丢掉：对端本来就不可信，不能让一条畸形消息把 UI 推进错误状态
            let Ok(payload) = serde_json::from_value::<PresencePayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            manager.publish(generation, |status| {
                status.remote_presence = Some(payload.state);
                status.peer_name = payload.display_name.clone();
            });

            manager
                .sink
                .emit(EVENT_PRESENCE, serde_json::to_value(payload).unwrap_or(Value::Null));

            Ok(None)
        }
        message_type::PET_STATE => {
            let Ok(snapshot) = serde_json::from_value::<PetSnapshot>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            manager.sink.emit(
                EVENT_PET_STATE,
                serde_json::to_value(snapshot.sanitized()).unwrap_or(Value::Null),
            );

            Ok(None)
        }
        message_type::STATS => {
            let Ok(stats) = serde_json::from_value::<InputStats>(envelope.payload.clone()) else {
                return Ok(None);
            };

            manager.publish(generation, |status| {
                status.remote_stats = Some(stats.clone());
            });

            manager
                .sink
                .emit(EVENT_STATS, serde_json::to_value(stats).unwrap_or(Value::Null));

            Ok(None)
        }
        _ => {
            manager
                .sink
                .emit(EVENT_MESSAGE, serde_json::to_value(envelope).unwrap_or(Value::Null));

            Ok(None)
        }
    }
}

fn handle_server_frame(
    manager: &Arc<PairManager>,
    generation: u64,
    text: &str,
) -> Result<(), PairFailure> {
    // R18：text 只用于服务端控制帧，未知或畸形的控制帧按忽略处理。
    // 当成致命错误会让我们在中继新增一条控制帧时反复断线重连。
    let Ok(frame) = serde_json::from_str::<ServerFrame>(text) else {
        tauri_plugin_log::log::warn!("忽略无法解析的中继控制帧");

        return Ok(());
    };

    match frame {
        ServerFrame::Welcome {
            protocol,
            peer_online,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(PairFailure {
                    message: format!("中继协议版本不匹配: {protocol}"),
                    fatal: true,
                });
            }

            publish_peer(manager, generation, peer_online);

            Ok(())
        }
        ServerFrame::Peer { online, device_id } => {
            if device_id == manager.device_id() {
                return Ok(());
            }

            publish_peer(manager, generation, online);

            Ok(())
        }
        ServerFrame::Error { code, message } => {
            manager.emit_error(generation, format!("中继错误 {code}: {message}"));

            Ok(())
        }
    }
}

fn publish_peer(manager: &Arc<PairManager>, generation: u64, online: bool) {
    manager.publish(generation, |status| {
        status.peer_online = online;
        status.state = if online {
            PairConnectionState::Connected
        } else {
            PairConnectionState::ConnectedPeerOffline
        };
    });

    manager
        .sink
        .emit(EVENT_PEER_CHANGED, json!({ "online": online }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pair::protocol::{PetKeyboardState, PetPointerState};

    const ROOT_KEY: [u8; 32] = [11u8; 32];

    #[derive(Default)]
    struct TestSink {
        log: Mutex<Vec<(String, Value)>>,
    }

    impl TestSink {
        fn payloads(&self, event: &str) -> Vec<Value> {
            self.log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter(|(name, _)| name == event)
                .map(|(_, payload)| payload.clone())
                .collect()
        }
    }

    impl PairEventSink for TestSink {
        fn emit(&self, event: &str, payload: Value) {
            self.log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((event.to_string(), payload));
        }
    }

    fn test_manager() -> (Arc<PairManager>, Arc<TestSink>) {
        let sink = Arc::new(TestSink::default());
        let manager = Arc::new(PairManager::new("test-device".into(), sink.clone()));

        (manager, sink)
    }

    /// 伪造一条来自对端的加密帧，交给 `handle_binary` 处理
    fn deliver(
        manager: &Arc<PairManager>,
        state: &mut SessionState,
        kind: FrameKind,
        envelope: &AppEnvelope,
    ) -> Result<Reply, String> {
        let frame = PairCipher::new(&ROOT_KEY)
            .seal(
                &FrameHeader::new(kind, 0),
                &envelope.to_bytes().unwrap(),
            )
            .unwrap();

        handle_binary(manager, 0, state, &frame)
    }

    #[test]
    fn validates_device_ids() {
        assert!(is_valid_device_id("9f1c2f2e-1f4c-4a0a-9c3d-1a2b3c4d5e6f"));
        assert!(!is_valid_device_id(""));
        assert!(!is_valid_device_id("with space"));
        assert!(!is_valid_device_id(&"a".repeat(65)));
    }

    #[test]
    fn backoff_grows_then_caps_with_jitter() {
        let mut backoff = Backoff::new();

        for expected_secs in BACKOFF_STEPS_SECS {
            let delay = backoff.next_delay().as_millis() as f64 / 1000.0;
            let base = expected_secs as f64;

            assert!(
                delay >= base * 0.8 && delay <= base * 1.2,
                "delay {delay} 超出 {base} 的 ±20%"
            );
        }

        let capped = backoff.next_delay().as_millis() as f64 / 1000.0;

        assert!(capped >= 24.0 && capped <= 36.0);
    }

    #[test]
    fn replaceable_frames_keep_only_the_latest_per_kind() {
        let mut state = SessionState::new(&[1u8; 32]);

        for index in 0..10 {
            state
                .queue(
                    FrameKind::PetState,
                    &AppEnvelope::new(message_type::PET_STATE, index, json!({ "index": index })),
                    true,
                )
                .unwrap();
            state
                .queue(
                    FrameKind::Stats,
                    &AppEnvelope::new(message_type::STATS, index, json!({ "index": index })),
                    true,
                )
                .unwrap();
        }

        assert!(state.reliable.is_empty());
        // 统计快照不能把还没发出去的宠物快照顶掉：两类各自留一帧
        assert_eq!(state.replaceable.len(), 2);
        assert!(
            state
                .replaceable
                .contains_key(&FrameKind::PetState.as_byte())
        );
        assert!(state.replaceable.contains_key(&FrameKind::Stats.as_byte()));
    }

    #[test]
    fn reliable_queue_reports_when_it_drops_the_oldest() {
        let mut state = SessionState::new(&[2u8; 32]);

        for index in 0..RELIABLE_QUEUE_LIMIT {
            let dropped = state
                .queue(
                    FrameKind::Chat,
                    &AppEnvelope::new(message_type::PING, index as u64, json!({})),
                    false,
                )
                .unwrap();

            assert!(!dropped, "第 {index} 条不应该触发丢弃");
        }

        let dropped = state
            .queue(
                FrameKind::Chat,
                &AppEnvelope::new(message_type::PING, u64::MAX, json!({})),
                false,
            )
            .unwrap();

        // 静默丢消息会让聊天永久缺一条；这里必须让调用方拿到信号
        assert!(dropped, "超出上限时必须报告丢弃");
        assert_eq!(state.reliable.len(), RELIABLE_QUEUE_LIMIT);
    }

    #[test]
    fn oversized_outbound_frames_are_rejected_locally() {
        let mut state = SessionState::new(&[3u8; 32]);
        let envelope = AppEnvelope::new(
            "pair.chat.text",
            1,
            json!({ "text": "a".repeat(MAX_BINARY_FRAME_SIZE) }),
        );

        let error = state.queue(FrameKind::Chat, &envelope, false).unwrap_err();

        assert!(error.contains("中继允许的大小"), "实际错误: {error}");
    }

    #[test]
    fn frame_sequence_increases_per_frame() {
        let mut state = SessionState::new(&[3u8; 32]);
        let envelope = AppEnvelope::new(message_type::PING, 0, json!({}));

        let first = state.encode(FrameKind::Ping, &envelope).unwrap();
        let second = state.encode(FrameKind::Ping, &envelope).unwrap();

        assert_eq!(FrameHeader::decode(&first).unwrap().seq, 0);
        assert_eq!(FrameHeader::decode(&second).unwrap().seq, 1);
        assert_ne!(first, second);
    }

    #[test]
    fn pending_replaceable_is_coalesced_until_the_task_takes_it() {
        let (manager, _sink) = test_manager();
        let (sender, mut receiver) = mpsc::unbounded_channel();

        *PairManager::lock(&manager.sender) = Some(sender);

        let snapshot = serde_json::to_value(PetSnapshot::default()).unwrap();

        manager
            .send_replaceable(FrameKind::PetState, message_type::PET_STATE, snapshot.clone())
            .unwrap();
        manager
            .send_replaceable(
                FrameKind::Stats,
                message_type::STATS,
                json!({ "date": "2026-09-23" }),
            )
            .unwrap();
        manager
            .send_replaceable(FrameKind::PetState, message_type::PET_STATE, snapshot)
            .unwrap();

        // 三帧只排一个唤醒：socket 卡住时过期的宠物快照不会堆在命令通道里
        assert!(matches!(receiver.try_recv(), Ok(Command::FlushReplaceable)));
        assert!(receiver.try_recv().is_err());

        let taken = manager.take_pending_replaceable();

        assert_eq!(taken.len(), 2);

        // 取走之后必须能再次唤醒，否则后续状态会永远发不出去
        manager
            .send_replaceable(FrameKind::PetState, message_type::PET_STATE, json!({}))
            .unwrap();

        assert!(matches!(receiver.try_recv(), Ok(Command::FlushReplaceable)));
    }

    #[test]
    fn ping_is_answered_with_pong() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new(message_type::PING, 1, json!({ "sentAt": 1 }));

        let reply = deliver(&manager, &mut state, FrameKind::Ping, &envelope)
            .unwrap()
            .expect("ping 应该回一条 pong");

        assert_eq!(reply.0, FrameKind::Ping);
        assert_eq!(reply.1.message_type, message_type::PONG);
        assert_eq!(reply.1.payload["sentAt"], 1);
        assert!(sink.payloads(EVENT_MESSAGE).is_empty());
    }

    #[test]
    fn duplicate_message_ids_are_dropped() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new(
            message_type::PRESENCE,
            2,
            json!({ "state": "active", "displayName": "A" }),
        );

        deliver(&manager, &mut state, FrameKind::Presence, &envelope).unwrap();

        assert_eq!(sink.payloads(EVENT_PRESENCE).len(), 1);

        // 重连补发会带来重复消息，只能让 UI 看到一次
        assert!(
            deliver(&manager, &mut state, FrameKind::Presence, &envelope)
                .unwrap()
                .is_none()
        );
        assert_eq!(sink.payloads(EVENT_PRESENCE).len(), 1);
    }

    #[test]
    fn malformed_presence_is_ignored_instead_of_breaking_the_connection() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new(message_type::PRESENCE, 3, json!({ "state": "bogus" }));

        let result = deliver(&manager, &mut state, FrameKind::Presence, &envelope).unwrap();

        assert!(result.is_none());
        assert!(sink.payloads(EVENT_PRESENCE).is_empty());
    }

    #[test]
    fn unknown_message_types_go_to_the_generic_event() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new("pair.chat.text", 4, json!({ "text": "hi" }));

        assert!(
            deliver(&manager, &mut state, FrameKind::Chat, &envelope)
                .unwrap()
                .is_none()
        );

        let payloads = sink.payloads(EVENT_MESSAGE);

        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["type"], "pair.chat.text");
    }

    #[test]
    fn welcome_with_a_different_protocol_version_is_fatal() {
        let (manager, sink) = test_manager();
        let version = PROTOCOL_VERSION + 1;
        let text = json!({
            "type": "server.welcome",
            "protocol": version,
            "peerOnline": false,
        })
        .to_string();

        let failure = handle_server_frame(&manager, 0, &text).unwrap_err();

        // 中继协议对不上是确定性错误，必须停下而不是无限重连
        assert!(failure.fatal);
        assert!(failure.message.contains("协议版本"));
        // 致命错误由会话层统一 publish 成 Error，这里不该先广播一个假的连接状态
        assert!(sink.payloads(EVENT_CONNECTION_CHANGED).is_empty());
    }

    #[test]
    fn malformed_server_control_frames_are_ignored() {
        let (manager, sink) = test_manager();

        // R18：中继将来新增控制帧时，老客户端只能忽略它，不能断线重连
        for text in ["not json at all", r#"{"type":"server.future"}"#, ""] {
            assert!(
                handle_server_frame(&manager, 0, text).is_ok(),
                "无法识别的控制帧应当被忽略: {text}"
            );
        }

        assert!(sink.payloads(EVENT_CONNECTION_CHANGED).is_empty());
        assert!(sink.payloads(EVENT_PEER_CHANGED).is_empty());
    }

    #[test]
    fn pet_state_is_sanitized_before_it_reaches_the_ui() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let snapshot = PetSnapshot {
            keyboard: PetKeyboardState {
                active: true,
                left_hand: true,
                right_hand: false,
                intensity: 2.0,
            },
            pointer: PetPointerState {
                active: true,
                x: 4.0,
                y: -1.0,
                speed: 0.53,
                left_down: true,
                right_down: false,
            },
        };
        let envelope = AppEnvelope::new(
            message_type::PET_STATE,
            5,
            serde_json::to_value(snapshot).unwrap(),
        );

        deliver(&manager, &mut state, FrameKind::PetState, &envelope).unwrap();

        let payloads = sink.payloads(EVENT_PET_STATE);

        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["keyboard"]["intensity"], 1.0);
        assert_eq!(payloads[0]["pointer"]["x"], 1.0);
        assert_eq!(payloads[0]["pointer"]["y"], 0.0);

        // f32 → JSON 会有二进制浮点误差，比较时留一点余量
        let speed = payloads[0]["pointer"]["speed"].as_f64().unwrap();

        assert!((speed - 0.55).abs() < 1e-6, "实际速度: {speed}");
    }

    #[test]
    fn stats_are_stored_in_the_status_and_broadcast() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let stats = InputStats {
            date: "2026-09-23".into(),
            today_keyboard: 5,
            today_mouse: 1,
            total_keyboard: 9,
            total_mouse: 2,
            share: true,
        };
        let envelope = AppEnvelope::new(
            message_type::STATS,
            6,
            serde_json::to_value(&stats).unwrap(),
        );

        deliver(&manager, &mut state, FrameKind::Stats, &envelope).unwrap();

        assert_eq!(manager.status().remote_stats, Some(stats));
        assert_eq!(sink.payloads(EVENT_STATS)[0]["todayKeyboard"], 5);
    }

    #[test]
    fn repeated_errors_are_reported_once_until_the_message_changes() {
        let (manager, sink) = test_manager();

        manager.emit_error(0, "连接失败: timeout".to_string());
        manager.emit_error(0, "连接失败: timeout".to_string());

        assert_eq!(sink.payloads(EVENT_ERROR).len(), 1);

        manager.emit_error(0, "连接失败: refused".to_string());

        assert_eq!(sink.payloads(EVENT_ERROR).len(), 2);
    }

    #[test]
    fn fatal_failure_stops_the_session_and_shows_the_error_state() {
        let (manager, sink) = test_manager();
        let (sender, _receiver) = mpsc::unbounded_channel();

        *PairManager::lock(&manager.sender) = Some(sender);

        manager.fail_hard(0, "鉴权失败：Pair Secret 与部署时的值不一致".to_string());

        let status = manager.status();

        assert_eq!(status.state, PairConnectionState::Error);
        assert_eq!(
            status.last_error.as_deref(),
            Some("鉴权失败：Pair Secret 与部署时的值不一致")
        );
        assert_eq!(sink.payloads(EVENT_ERROR).len(), 1);
        // 旧任务的 sender 已经被清掉：后续发送必须报错，而不是静默成功
        assert!(
            manager
                .send(FrameKind::Ping, message_type::PING, json!({}))
                .is_err()
        );
    }
}

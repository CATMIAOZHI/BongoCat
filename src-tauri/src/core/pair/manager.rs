//! PairManager：连接生命周期、状态机、重连、发送队列与事件广播。
//!
//! 整个应用只有这一个网络连接：所有 WebView 共享同一份 managed state，前端只观察状态。
//!
//! 事件通过 [`PairEventSink`] 发出，而不是直接依赖 `AppHandle`，这样状态机可以在
//! 测试里用真实中继跑完整流程（见 e2e.rs）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager as _, Runtime};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::client::{self, PairSocket};
use super::crypto::{self, PairCipher};
use super::protocol::{
    AppEnvelope, FrameHeader, FrameKind, MAX_BINARY_FRAME_SIZE, PROTOCOL_VERSION, PresencePayload,
    PresenceState, RecentMessageIds, ServerFrame, message_type,
};
use super::secret;

pub const EVENT_CONNECTION_CHANGED: &str = "pair-connection-changed";
pub const EVENT_PEER_CHANGED: &str = "pair-peer-changed";
pub const EVENT_PRESENCE: &str = "pair-presence";
pub const EVENT_MESSAGE: &str = "pair-message";
pub const EVENT_ERROR: &str = "pair-error";

const RELIABLE_QUEUE_LIMIT: usize = 512;
const RECENT_MESSAGE_LIMIT: usize = 256;
pub const HEARTBEAT_ENV: &str = "BONGO_PAIR_HEARTBEAT_SECS";
const DEFAULT_HEARTBEAT_SECS: u64 = 60;
const BACKOFF_STEPS_SECS: [u64; 5] = [1, 2, 5, 10, 30];

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

#[derive(Debug, Clone)]
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
        replaceable: bool,
    },
    Disconnect,
}

pub struct PairManager {
    status: Mutex<PairStatus>,
    sender: Mutex<Option<mpsc::UnboundedSender<Command>>>,
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
                device_id,
                relay_url: None,
                last_error: None,
            }),
            sender: Mutex::new(None),
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
        });
    }

    pub fn send(&self, kind: FrameKind, message_type: &str, payload: Value) -> Result<(), String> {
        self.enqueue(kind, message_type, payload, false)
    }

    /// 可覆盖的实时状态（宠物快照、统计）：拥塞时新数据直接覆盖旧数据
    #[allow(dead_code)]
    pub fn send_replaceable(
        &self,
        kind: FrameKind,
        message_type: &str,
        payload: Value,
    ) -> Result<(), String> {
        self.enqueue(kind, message_type, payload, true)
    }

    fn enqueue(
        &self,
        kind: FrameKind,
        message_type: &str,
        payload: Value,
        replaceable: bool,
    ) -> Result<(), String> {
        let sender = Self::lock(&self.sender)
            .clone()
            .ok_or_else(|| "当前没有连接".to_string())?;

        let envelope = AppEnvelope::new(message_type, self.next_envelope_seq(), payload);

        sender
            .send(Command::Send {
                kind,
                envelope,
                replaceable,
            })
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

    fn emit_error(&self, generation: u64, message: String) {
        if generation != self.generation() {
            return;
        }

        // 除了单独的错误事件，也写进 status.lastError，让前端打开偏好页时能直接看到原因
        self.publish(generation, |status| {
            status.last_error = Some(message.clone());
        });

        self.sink.emit(EVENT_ERROR, json!({ "message": message }));
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
    replaceable: Option<Vec<u8>>,
    frame_seq: u32,
    recent: RecentMessageIds,
}

impl SessionState {
    fn new(root_key: &[u8; 32]) -> Self {
        Self {
            cipher: PairCipher::new(root_key),
            reliable: VecDeque::new(),
            replaceable: None,
            frame_seq: 0,
            recent: RecentMessageIds::new(RECENT_MESSAGE_LIMIT),
        }
    }

    fn encode(&mut self, kind: FrameKind, envelope: &AppEnvelope) -> Result<Vec<u8>, String> {
        let plaintext = envelope.to_bytes()?;
        let header = FrameHeader::new(kind, self.frame_seq);

        self.frame_seq = self.frame_seq.wrapping_add(1);

        self.cipher.seal(&header, &plaintext)
    }

    fn queue(
        &mut self,
        kind: FrameKind,
        envelope: &AppEnvelope,
        replaceable: bool,
    ) -> Result<(), String> {
        let frame = self.encode(kind, envelope)?;

        if replaceable {
            // 实时状态 latest wins：绝不无限堆积
            self.replaceable = Some(frame);
        } else {
            if self.reliable.len() >= RELIABLE_QUEUE_LIMIT {
                self.reliable.pop_front();
            }

            self.reliable.push_back(frame);
        }

        Ok(())
    }
}

enum Outcome {
    Stopped,
    Lost(String),
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

        match client::connect(&config.relay_url, &config.auth_token, &config.device_id).await {
            Ok(socket) => {
                backoff.reset();

                manager.publish(generation, |status| {
                    status.state = PairConnectionState::ConnectedPeerOffline;
                    status.peer_online = false;
                    status.last_error = None;
                });

                match live(&manager, generation, &mut state, socket, &mut receiver).await {
                    Outcome::Stopped => break,
                    Outcome::Lost(reason) => manager.emit_error(generation, reason),
                }
            }
            Err(error) => manager.emit_error(generation, error),
        }

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
                    Some(Command::Send { kind, envelope, replaceable }) => {
                        if let Err(error) = state.queue(kind, &envelope, replaceable) {
                            manager.emit_error(generation, error);
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
        return Outcome::Lost(error);
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
                    let _ = sink.send(Message::Close(None)).await;

                    return Outcome::Stopped;
                }
                Some(Command::Send { kind, envelope, replaceable }) => {
                    match state.queue(kind, &envelope, replaceable) {
                        Err(error) => manager.emit_error(generation, error),
                        Ok(()) => {
                            if let Err(error) = flush(&mut sink, state).await {
                                return Outcome::Lost(error);
                            }
                        }
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
                    Err(error) => return Outcome::Lost(format!("连接错误: {error}")),
                    Ok(Message::Binary(bytes)) => {
                        if bytes.len() > MAX_BINARY_FRAME_SIZE {
                            return Outcome::Lost("收到超过上限的帧".into());
                        }

                        match handle_binary(manager, generation, state, &bytes) {
                            Err(error) => manager.emit_error(generation, error),
                            Ok(Some((kind, reply))) => {
                                match state.queue(kind, &reply, false) {
                                    Err(error) => manager.emit_error(generation, error),
                                    Ok(()) => {
                                        if let Err(error) = flush(&mut sink, state).await {
                                            return Outcome::Lost(error);
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
                    return Outcome::Lost("心跳超时".into());
                }

                awaiting_pong = true;

                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Outcome::Lost("发送心跳失败".into());
                }
            },
        }
    }
}

/// 中继主动关闭时，把关闭码翻译成人能看懂的原因（取值见 server-cloudflare/src/protocol.ts）
fn describe_close(code: Option<u16>) -> String {
    match code {
        Some(4002) => "这条连接被同一台设备的新连接顶替".to_string(),
        Some(4003) => "配对已满：对端已经在另一个位置连上了".to_string(),
        Some(4004) => "旧连接因长时间没有活动被顶替".to_string(),
        Some(1008) => "中继判定帧格式或发送频率异常".to_string(),
        Some(1009) => "帧超过中继允许的大小".to_string(),
        Some(1011) => "中继内部错误".to_string(),
        Some(code) => format!("中继关闭了连接（{code}）"),
        None => "中继关闭了连接".to_string(),
    }
}

async fn flush<S>(sink: &mut S, state: &mut SessionState) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    while let Some(frame) = state.reliable.pop_front() {
        if let Err(error) = sink.send(Message::Binary(frame.clone().into())).await {
            state.reliable.push_front(frame);

            return Err(format!("发送失败: {error}"));
        }
    }

    if let Some(frame) = state.replaceable.take()
        && let Err(error) = sink.send(Message::Binary(frame.clone().into())).await
    {
        state.replaceable = Some(frame);

        return Err(format!("发送失败: {error}"));
    }

    Ok(())
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
            let payload: PresencePayload = serde_json::from_value(envelope.payload.clone())
                .map_err(|err| format!("Presence 载荷不合法: {err}"))?;

            manager.publish(generation, |status| {
                status.remote_presence = Some(payload.state);
                status.peer_name = payload.display_name.clone();
            });

            manager
                .sink
                .emit(EVENT_PRESENCE, serde_json::to_value(payload).unwrap_or(Value::Null));

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
) -> Result<(), String> {
    let frame: ServerFrame =
        serde_json::from_str(text).map_err(|err| format!("中继控制帧不合法: {err}"))?;

    match frame {
        ServerFrame::Welcome {
            protocol,
            peer_online,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(format!("中继协议版本不匹配: {protocol}"));
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
    fn replaceable_frames_are_overwritten_instead_of_queued() {
        let mut state = SessionState::new(&[1u8; 32]);

        for _ in 0..10 {
            state
                .queue(
                    FrameKind::PetState,
                    &AppEnvelope::new(message_type::PING, 0, json!({})),
                    true,
                )
                .unwrap();
        }

        assert!(state.reliable.is_empty());
        assert!(state.replaceable.is_some());
    }

    #[test]
    fn reliable_queue_is_bounded() {
        let mut state = SessionState::new(&[2u8; 32]);

        for index in 0..(RELIABLE_QUEUE_LIMIT + 10) {
            state
                .queue(
                    FrameKind::Chat,
                    &AppEnvelope::new(message_type::PING, index as u64, json!({})),
                    false,
                )
                .unwrap();
        }

        assert_eq!(state.reliable.len(), RELIABLE_QUEUE_LIMIT);
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
}

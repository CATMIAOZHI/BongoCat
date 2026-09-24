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
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager as _, Runtime};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::client::{self, PairFailure};
use super::crypto::{self, PairCipher};
use super::history::{
    ChatMessage, MESSAGE_TEXT_LIMIT, MessageDirection, MessageKind, MessageStatus, NewAttachment,
    NewMessage, PairHistory,
};
use super::link;
use super::protocol::{
    AppEnvelope, ChatAckPayload, ChatTextPayload, FrameHeader, FrameKind, InputStats,
    MAX_BINARY_FRAME_SIZE, PROTOCOL_VERSION, PairSignalPayload, PetSnapshot, PresencePayload,
    PresenceState, RecentMessageIds, RelayConfig, RelayLimits, ServerFrame, TransferIdPayload,
    TransferKind, TransferOfferPayload, TransferRejectPayload, TransferVerifiedPayload,
    message_type, now_millis,
};
use super::secret;
use super::transfer::{
    CHUNK_SIZE, DEFAULT_MAX_SIZE, IncomingTransfer, OutgoingTransfer, P2P_CHUNK_SIZE,
    TransferStore, chunk_count, chunk_size_is_valid, clamp_limit, needs_confirmation,
    sanitize_file_name, sanitize_mime,
};

pub const EVENT_CONNECTION_CHANGED: &str = "pair-connection-changed";
pub const EVENT_PEER_CHANGED: &str = "pair-peer-changed";
pub const EVENT_PRESENCE: &str = "pair-presence";
pub const EVENT_PET_STATE: &str = "pair-pet-state";
pub const EVENT_STATS: &str = "pair-stats";
pub const EVENT_MESSAGE: &str = "pair-message";
pub const EVENT_ERROR: &str = "pair-error";
/// 收到一条新消息（§47）：聊天窗口据此追加，远端猫据此做 Q 弹/闪光/提示音
pub const EVENT_MESSAGE_RECEIVED: &str = "pair-message-received";
/// 已有消息的状态变化（sent / delivered / failed）
pub const EVENT_MESSAGE_UPDATED: &str = "pair-message-updated";
/// 附件传输进度（发送与接收共用）。载荷见 [`TransferProgress`]。
pub const EVENT_TRANSFER: &str = "pair-transfer";

const RELIABLE_QUEUE_LIMIT: usize = 512;
/// 一次重连最多补发多少条历史消息，避免对方一上线就被灌满
const CHAT_RESEND_LIMIT: usize = 100;
/// 每个 transfer 最多多久报一次进度（§40：进度要有，但别把事件刷爆）
const TRANSFER_PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// 同时进行的附件传输上限：对端不能靠一堆 offer 把内存或磁盘撑爆
const MAX_ACTIVE_TRANSFERS: usize = 4;
/// 出站节奏（R18）：中继每个 socket 只给 30 帧/秒（桶容量同为 30）。一次性补发几十条
/// 离线消息会把桶扣穿、被 `close 1008` 断开，而重连后又补发同一批，变成「连上就被踢」的
/// 空转，ack 也永远收不到。客户端主动按 20 帧/秒放行，给心跳与实时快照留出余量。
const OUTBOUND_FRAMES_PER_SECOND: f64 = 20.0;
const OUTBOUND_BURST: f64 = 20.0;
/// 附件分片单独一套额度（R18 / §40）。中继分片桶是 20 个/秒、容量 20，与「20 帧/秒」的
/// 通用额度贴得死死的：客户端按 20/s 发就是零余量，到达间隔被网络抖动压到 50ms 以下
/// （或两枚令牌被压进同一瞬间）就会把中继的桶扣穿，传输中途 `close 1008`。
/// 这里主动降到 15 个/秒、突发 10，留出余量；512 KiB × 15 ≈ 7.5 MiB/s，仍然远快于
/// 家用上行，用户感知不到差别。
const OUTBOUND_CHUNKS_PER_SECOND: f64 = 15.0;
const OUTBOUND_CHUNK_BURST: f64 = 10.0;
/// DC 那条腿自己的帧额度（R23）：P2P 下没有中继的令牌桶可依，这条腿的预算就是 §6 的
/// 60Hz 上限本身。它**不是** `OUTBOUND_*` 的缩放，也不参与 `retune`——DC 上的帧压根
/// 不经过中继的计费点，拿中继额度去压它正好会毁掉 60Hz。
const DIRECT_FRAMES_PER_SECOND: f64 = 60.0;
const DIRECT_BURST: f64 = 60.0;
/// DC 上的**附件分片**另有一套额度（§7 / R23 / R32）：绝不能和 60Hz 的快照共用桶，
/// 否则 48 KiB 的块会把 60 枚/秒吃光、对端猫在整段传输里冻住。
///
/// 数值取「与中继那条路同样的字节速率」：中继是 512 KiB × 15 ≈ 7.5 MiB/s，DC 的分片
/// 小 512/48 倍，速率就按同一比例放大（15 × 512 ÷ 48 = 160）。它**不是**中继的桶，
/// 没有 20 个/秒的平台上限，所以放大是安全的。
const DIRECT_CHUNKS_PER_SECOND: f64 = 160.0;
const DIRECT_CHUNK_BURST: f64 = 16.0;
/// Direct 那条路上「这一轮发不出去」时的轮询间隔（R32）。
///
/// 附件分片那条 `select!` 分支有个不变量：`chunk_wait == ZERO` 必须意味着紧接着一定
/// 发得出去，否则就会在 `Ok(false)` 与 `ZERO` 之间空转。背压（`writable`）不像令牌那样
/// 有可计算的剩余时间，所以它翻假时用这个固定间隔轮询。
const DIRECT_RETRY_INTERVAL: Duration = Duration::from_millis(5);
/// 桌宠快照的发送上限（§6 / R23）：只有当前生效传输能承载时才提上去。
const MAX_PET_STATE_HZ: f64 = 60.0;
/// R4 以来（也就是 v1）的缺省快照上限：传输承载不了 60Hz 时保持它，
/// 所以 CF 版拿到缺省推导值（30 × 2/3 = 20）时行为与今天一字不变。
const DEFAULT_PET_STATE_HZ: f64 = 3.0;
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
/// 等中继握手后的第一帧（`server.welcome`）的上限：它本该立刻到达
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// P2P 这条腿的状态（R21 / R28）。**只影响显示与 Phase 8c 的切换决策**：中继上的
    /// 功能（聊天、附件、语音、信令、重连）与它无关，所以它失败时用户不该看到任何降级。
    pub p2p: P2pState,
    /// 前端该按多少 Hz 发桌宠快照（§6 / R23）。由**当前生效传输**的额度决定，见
    /// [`pet_state_hz`]：P2P 下是 60，自建中继广告额度够时是 60，其余（含 CF 缺省）是 3。
    /// 前端只消费这个数字，不再自己判断该用哪个上限。
    pub pet_state_hz: f64,
}

/// P2P 这条腿的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum P2pState {
    /// 没在协商：对端没声明支持 P2P，或者这一轮还没开始
    Off,
    /// 正在协商 / ICE 打洞（10~30 秒，对用户不可见）
    Connecting,
    /// DataChannel 可用，可覆盖流可以切过去
    Connected,
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
    /// 开始发送一个已经入库的附件（§38 的 offer）
    StartTransfer(Box<OutgoingRequest>),
    /// 接收方同意接收（§42：超过阈值的大文件要用户点一下）
    AcceptTransfer { transfer_id: u64 },
    /// 接收方拒绝接收
    RejectTransfer { transfer_id: u64 },
    /// 任意一端取消
    CancelTransfer { transfer_id: u64 },
    Disconnect,
}

/// 一次「开始发送附件」的请求。附件记录与消息行都已经写进本地库，这里只带发送所需的信息。
#[derive(Debug, Clone)]
pub struct OutgoingRequest {
    pub transfer_id: u64,
    pub message_id: String,
    pub attachment_id: String,
    pub kind: TransferKind,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    pub path: std::path::PathBuf,
}

/// 附件传输进度事件（`pair-transfer`）的载荷
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub transfer_id: u64,
    pub message_id: String,
    pub attachment_id: String,
    pub kind: TransferKind,
    pub name: String,
    pub size: u64,
    pub transferred: u64,
    /// 0..100
    pub percent: u8,
    pub direction: MessageDirection,
    /// `waiting`（等对方接收）/ `sending` / `receiving` / `done` / `failed` / `canceled`
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 传输在会话里的阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferPhase {
    /// 发送方：offer 已发出，等 accept
    AwaitingAccept,
    /// 发送方：正在发分片
    Sending,
    /// 接收方：等用户确认（大文件，§42）
    AwaitingDecision,
    /// 接收方：正在收分片
    Receiving,
}

impl TransferPhase {
    const fn is_outgoing(self) -> bool {
        matches!(self, Self::AwaitingAccept | Self::Sending)
    }
}

/// 一次附件传输在本会话里的状态
struct TransferSession {
    id: u64,
    message_id: String,
    attachment_id: String,
    kind: TransferKind,
    name: String,
    mime: String,
    size: u64,
    sha256: String,
    chunk_size: u32,
    chunks: u32,
    phase: TransferPhase,
    /// 这一单钉在哪条腿上（§8 Phase 10）：offer 时定下来，全程不改。
    route: link::Route,
    outgoing: Option<OutgoingTransfer>,
    incoming: Option<IncomingTransfer>,
    /// 进度节流：每个 transfer 最多 150ms 报一次
    last_progress_at: Option<tokio::time::Instant>,
}

impl TransferSession {
    fn direction(&self) -> MessageDirection {
        if self.phase.is_outgoing() {
            MessageDirection::Outgoing
        } else {
            MessageDirection::Incoming
        }
    }

    fn transferred(&self) -> u64 {
        match (&self.outgoing, &self.incoming) {
            (Some(outgoing), _) => outgoing.bytes_sent,
            (_, Some(incoming)) => incoming.received_bytes,
            _ => 0,
        }
    }

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
    /// 单个附件的上限（字节）。设置页可以改，硬上限见 `transfer::HARD_MAX_SIZE`。
    max_attachment_size: AtomicU64,
    /// 正在进行的传输：`messageId` → `transferId`。
    ///
    /// 聊天 UI 手里只有消息 id，所以接受 / 拒绝 / 取消都用消息 id 定位，
    /// 不用把 transferId 存进数据库（那需要一次表结构迁移）。
    transfers: Mutex<HashMap<String, u64>>,
    history: Arc<PairHistory>,
    sink: Arc<dyn PairEventSink>,
    store: TransferStore,
}

impl PairManager {
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn new(
        device_id: String,
        sink: Arc<dyn PairEventSink>,
        history: Arc<PairHistory>,
        store: TransferStore,
    ) -> Self {
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
                p2p: P2pState::Off,
                pet_state_hz: DEFAULT_PET_STATE_HZ,
            }),
            sender: Mutex::new(None),
            pending: Mutex::new(PendingReplaceable::default()),
            generation: AtomicU64::new(0),
            envelope_seq: AtomicU64::new(0),
            max_attachment_size: AtomicU64::new(DEFAULT_MAX_SIZE),
            history,
            sink,
            store,
            transfers: Mutex::new(HashMap::new()),
        }
    }

    /// 本地聊天库。历史读取与导出等命令直接用它，不经过连接状态。
    pub fn history(&self) -> &Arc<PairHistory> {
        &self.history
    }

    /// 附件落盘位置（附件目录与临时目录）
    pub fn store(&self) -> &TransferStore {
        &self.store
    }

    /// 单个附件的上限（字节）
    pub fn max_attachment_size(&self) -> u64 {
        self.max_attachment_size.load(Ordering::SeqCst)
    }

    /// 设置单个附件的上限（MB）。返回夹紧之后真正的字节数。
    pub fn set_max_attachment_mb(&self, mb: u64) -> u64 {
        let bytes = clamp_limit(mb);

        self.max_attachment_size.store(bytes, Ordering::SeqCst);

        bytes
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
            // 新会话还没起腿：上一轮的 `Connected` 必须立刻消失，否则「立即连接」
            // 之后的十几秒里偏好页会显示「正在连接」+「已直连」
            status.p2p = P2pState::Off;
            // 同理，快照上限也回到缺省：新会话还没读到中继广告的额度
            status.pet_state_hz = DEFAULT_PET_STATE_HZ;
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
            // 会话没了，P2P 那条腿也跟着没了：不复位的话 UI 会一直显示「已直连」
            status.p2p = P2pState::Off;
            status.pet_state_hz = DEFAULT_PET_STATE_HZ;
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

    /// 发一条文本消息（§31）。
    ///
    /// 先写本地库再上网：连不上或发送失败时留在 `pending`，等对端上线后由
    /// [`Self::resend_pending_chat`] 重发（§32）。服务器不存，所以离线期间对方收不到。
    pub fn send_chat(self: &Arc<Self>, text: &str) -> Result<ChatMessage, String> {
        if text.trim().is_empty() {
            return Err("消息内容不能为空".to_string());
        }

        if text.len() > MESSAGE_TEXT_LIMIT {
            return Err(format!("单条消息最多 {} KiB", MESSAGE_TEXT_LIMIT / 1024));
        }

        let epoch = self.history.epoch()?;
        let message = self.history.insert(&NewMessage::outgoing_text(
            uuid::Uuid::new_v4().to_string(),
            text.to_string(),
            now_millis(),
            epoch,
        ))?;

        self.deliver_chat(&message);

        // 回读一次：发出去之后状态可能已经变成 sent
        Ok(self.history.find(&message.id)?.unwrap_or(message))
    }

    /// 尽力发一条已经入库的消息；发不出去就保持原状态，交给下次重连
    fn deliver_chat(self: &Arc<Self>, message: &ChatMessage) {
        let Some(text) = message.text.clone() else {
            return;
        };

        let payload = json!({ "messageId": message.id, "text": text });

        if self
            .enqueue(FrameKind::Chat, message_type::CHAT_TEXT, payload)
            .is_err()
        {
            return;
        }

        self.publish_message_status(&message.id, MessageStatus::Sent);
    }

    /// 状态真的变了才广播，避免重复事件把 UI 刷成重渲染
    fn publish_message_status(&self, id: &str, status: MessageStatus) {
        let Ok(Some(updated)) = self.history.set_status(id, status) else {
            return;
        };

        self.sink.emit(
            EVENT_MESSAGE_UPDATED,
            serde_json::to_value(updated).unwrap_or(Value::Null),
        );
    }

    /// 对端上线后补发还没送达的消息（§32）
    fn resend_pending_chat(self: &Arc<Self>) {
        let Ok(pending) = self.history.pending(CHAT_RESEND_LIMIT) else {
            tauri_plugin_log::log::warn!("读取待发送消息失败，本次不补发");

            return;
        };

        for message in pending {
            self.deliver_chat(&message);
        }
    }

    /// 把一次附件发送请求交给连接任务（附件与消息行已经落库）
    pub fn start_transfer(self: &Arc<Self>, request: OutgoingRequest) -> Result<(), String> {
        self.sender()?
            .send(Command::StartTransfer(Box::new(request)))
            .map_err(|_| "连接任务已结束".to_string())
    }

    /// 记下「这条消息正在传输」，命令层靠它把 messageId 翻译成 transferId
    fn register_transfer(&self, message_id: &str, transfer_id: u64) {
        Self::lock(&self.transfers).insert(message_id.to_string(), transfer_id);
    }

    fn unregister_transfer(&self, message_id: &str) {
        Self::lock(&self.transfers).remove(message_id);
    }

    fn transfer_of(&self, message_id: &str) -> Result<u64, String> {
        Self::lock(&self.transfers)
            .get(message_id)
            .copied()
            .ok_or_else(|| "这次传输已经结束了".to_string())
    }

    /// 接收方同意接收某个大文件（§42）
    pub fn accept_transfer(&self, message_id: &str) -> Result<(), String> {
        let transfer_id = self.transfer_of(message_id)?;

        self.sender()?
            .send(Command::AcceptTransfer { transfer_id })
            .map_err(|_| "连接任务已结束".to_string())
    }

    /// 接收方拒绝接收（§42）
    pub fn reject_transfer(&self, message_id: &str) -> Result<(), String> {
        let transfer_id = self.transfer_of(message_id)?;

        self.sender()?
            .send(Command::RejectTransfer { transfer_id })
            .map_err(|_| "连接任务已结束".to_string())
    }

    /// 任一端取消正在进行的传输（§43）
    pub fn cancel_transfer(&self, message_id: &str) -> Result<(), String> {
        let transfer_id = self.transfer_of(message_id)?;

        self.sender()?
            .send(Command::CancelTransfer { transfer_id })
            .map_err(|_| "连接任务已结束".to_string())
    }

    /// 发起方重试一条失败的附件消息（§43：UI 上的「重试」）。
    ///
    /// 只支持本机发出的附件：接收方的「重试」要请对方重发，协议里没有这条消息，
    /// 所以接收方只能显示失败原因。
    pub fn retry_attachment(self: &Arc<Self>, message_id: &str) -> Result<(), String> {
        let message = self
            .history
            .find(message_id)?
            .ok_or_else(|| "找不到这条消息".to_string())?;

        if message.direction != MessageDirection::Outgoing {
            return Err("这是对方发来的附件，需要对方重发".to_string());
        }

        let attachment = message
            .attachment
            .clone()
            .ok_or_else(|| "这条消息没有附件".to_string())?;
        let path = attachment
            .local_path
            .clone()
            .ok_or_else(|| "附件不在本机了，请重新选择文件".to_string())?;
        let path = std::path::PathBuf::from(path);

        if !path.exists() {
            return Err("附件不在本机了，请重新选择文件".to_string());
        }

        let size = attachment.size.unwrap_or_default();
        let sha256 = attachment.sha256.clone().unwrap_or_default();

        if size > self.max_attachment_size() {
            return Err("附件超过当前的大小上限".to_string());
        }

        let request = OutgoingRequest {
            transfer_id: new_transfer_id(),
            message_id: message.id.clone(),
            attachment_id: attachment.id.clone(),
            kind: TransferKind::parse(attachment.kind.as_str())?,
            name: attachment
                .original_name
                .clone()
                .unwrap_or_else(|| "attachment".to_string()),
            mime: attachment
                .mime
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            size,
            sha256,
            path,
        };

        // 重发前先回到「等待发送」，否则 UI 会一直显示上一次的失败
        self.publish_message_status(message_id, MessageStatus::Pending);

        self.start_transfer(request)
    }

    /// 广播传输进度（调用方已经做过节流）
    fn publish_transfer(&self, progress: &TransferProgress) {
        self.sink.emit(
            EVENT_TRANSFER,
            serde_json::to_value(progress).unwrap_or(Value::Null),
        );
    }

    /// 附件消息失败：更新状态并让 UI 知道原因。
    ///
    /// 命令层也要用它：发送请求根本没能交给连接任务时（没连着、同时传输太多），
    /// 消息必须落到 failed，否则它永远停在「等待发送」既不会重发也不能重试（§43）。
    pub(crate) fn fail_attachment(&self, message_id: &str, reason: &str) {
        self.mark_attachment_failed(message_id);

        self.sink
            .emit(EVENT_ERROR, json!({ "message": reason.to_string() }));
    }

    /// 只改状态、不报错：用户自己取消不是故障，偏好页不该因此留下一条红色的
    /// 「最近一次错误」（§43 只要求 UI 给出「重试」）
    pub(crate) fn mark_attachment_failed(&self, message_id: &str) {
        self.publish_message_status(message_id, MessageStatus::Failed);
    }

    /// 取走所有待发送的可覆盖状态（每次 flush 只取一次，取走后由连接任务负责送达）
    fn take_pending_replaceable(&self) -> Vec<(FrameKind, AppEnvelope)> {
        let mut pending = Self::lock(&self.pending);

        pending.queued = false;

        pending.entries.drain().map(|(_, item)| item).collect()
    }

    fn enqueue(&self, kind: FrameKind, message_type: &str, payload: Value) -> Result<(), String> {
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
            status.p2p = P2pState::Off;
            status.pet_state_hz = DEFAULT_PET_STATE_HZ;
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
    /// 派生 per-transfer 密钥要用（R17）
    root_key: [u8; 32],
    /// 可靠队列：帧与它对应的信封一起存，队列满时才知道挤掉的是哪条消息
    reliable: VecDeque<(Vec<u8>, AppEnvelope)>,
    /// 每种可覆盖类型各自最多留一帧（`BTreeMap` 同时保证发送顺序稳定）
    replaceable: BTreeMap<u8, Vec<u8>>,
    frame_seq: u32,
    recent: RecentMessageIds,
    /// 这一次连接里正在进行的附件传输，按 transferId 索引
    transfers: HashMap<u64, TransferSession>,
}

impl SessionState {
    fn new(root_key: &[u8; 32]) -> Self {
        Self {
            cipher: PairCipher::new(root_key),
            root_key: *root_key,
            reliable: VecDeque::new(),
            replaceable: BTreeMap::new(),
            frame_seq: 0,
            recent: RecentMessageIds::new(RECENT_MESSAGE_LIMIT),
            transfers: HashMap::new(),
        }
    }

    /// 每个 transfer 一把临时密钥（R17）。派生很便宜，就不做缓存了。
    fn transfer_cipher(&self, transfer_id: u64) -> PairCipher {
        PairCipher::new(&crypto::derive_transfer_key(&self.root_key, transfer_id))
    }

    /// 放进一次传输会话。超过上限就拒绝，避免对端用一堆 offer 撑爆内存与磁盘。
    fn open_transfer(&mut self, session: TransferSession) -> Result<(), String> {
        if self.transfers.contains_key(&session.id) {
            return Err("重复的 transferId".to_string());
        }

        if self.transfers.len() >= MAX_ACTIVE_TRANSFERS {
            return Err("同时进行的附件传输太多，请稍后再发".to_string());
        }

        self.transfers.insert(session.id, session);

        Ok(())
    }

    /// 还有没有「等着发分片」的发送方会话
    fn has_pending_chunks(&self) -> bool {
        self.transfers
            .values()
            .any(|session| session.phase == TransferPhase::Sending && !session_is_complete(session))
    }

    /// 下一次该发分片的 transfer（按 map 顺序稳定取第一个即可：并发传输数量很小）
    fn next_sending_transfer(&self) -> Option<u64> {
        self.transfers
            .iter()
            .find(|(_, session)| {
                session.phase == TransferPhase::Sending && !session_is_complete(session)
            })
            .map(|(id, _)| *id)
    }

    /// 下一次该发分片的那一单钉在哪条腿上（§8 Phase 10）。`None` = 没有待发分片。
    ///
    /// `chunk_wait_duration` 必须和 `send_next_chunk` 算出**同一个** transfer，否则
    /// 「`ZERO` 就意味着立刻发得出去」那条不变量会被打破（等的是中继的令牌、发的却是
    /// DC 那一单，于是空转）。
    fn next_sending_route(&self) -> Option<link::Route> {
        self.next_sending_transfer()
            .and_then(|transfer_id| self.transfers.get(&transfer_id))
            .map(|session| session.route)
    }

    /// 收下所有传输会话（连接结束时用来收尾）
    fn take_transfers(&mut self) -> Vec<TransferSession> {
        self.transfers.drain().map(|(_, session)| session).collect()
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

    /// 返回被挤掉的那一帧的信封（`None` 表示没有丢弃）。调用方要把它对应的消息退回
    /// 可重试状态：只报一句「队列已满」而让消息停在「已发送」，等于骗用户。
    fn queue(
        &mut self,
        kind: FrameKind,
        envelope: &AppEnvelope,
        replaceable: bool,
    ) -> Result<Option<AppEnvelope>, String> {
        let frame = self.encode(kind, envelope)?;

        if replaceable {
            // 实时状态 latest wins：每类只留最新一帧，绝不无限堆积
            self.replaceable.insert(kind.as_byte(), frame);

            return Ok(None);
        }

        let dropped = if self.reliable.len() >= RELIABLE_QUEUE_LIMIT {
            // 队头是最旧的一帧，连它的信封一起拿出来，才能找回对应的消息
            self.reliable.pop_front().map(|(_, dropped)| dropped)
        } else {
            None
        };

        self.reliable.push_back((frame, envelope.clone()));

        Ok(dropped)
    }
}

/// 发送方是不是把所有分片都发出去了
fn session_is_complete(session: &TransferSession) -> bool {
    session
        .outgoing
        .as_ref()
        .map(|outgoing| outgoing.is_done())
        .unwrap_or(true)
}

/// 传输 id：8 字节随机数。同时用于派生 per-transfer 密钥，两端各生成一个即可，撞号概率可忽略。
pub fn new_transfer_id() -> u64 {
    let mut bytes = [0u8; 8];

    rand::rng().fill_bytes(&mut bytes);

    match u64::from_be_bytes(bytes) {
        0 => 1,
        value => value,
    }
}

/// 出站节奏控制器（R18）。令牌按时间连续补充，容量等于突发上限。
///
/// 速率与容量都是参数：应用帧与附件分片各用一套（见 `OUTBOUND_*` 常量）。
struct Pacer {
    tokens: f64,
    updated_at: tokio::time::Instant,
    rate: f64,
    burst: f64,
}

impl Pacer {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            updated_at: tokio::time::Instant::now(),
            rate,
            burst,
        }
    }

    /// 按经过的时间补充令牌，但不允许攒成无限突发（纯函数，便于单测）
    fn refill(tokens: f64, elapsed_secs: f64, rate: f64, burst: f64) -> f64 {
        (tokens + elapsed_secs.max(0.0) * rate).min(burst)
    }

    /// 取一枚令牌；没有就等到下一枚补充出来
    async fn acquire(&mut self) {
        loop {
            self.refill_now();

            if self.tokens >= 1.0 {
                self.tokens -= 1.0;

                return;
            }

            let wait = (1.0 - self.tokens) / self.rate;

            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    /// 按当前时间把令牌补上（取令牌前必做）
    fn refill_now(&mut self) {
        let now = tokio::time::Instant::now();
        let elapsed = now.duration_since(self.updated_at).as_secs_f64();

        self.updated_at = now;
        self.tokens = Self::refill(self.tokens, elapsed, self.rate, self.burst);
    }

    /// 同时取两套额度：**两边都够才一起扣**。
    ///
    /// 附件分片要同时过通用帧额度与分片额度，分两次取会出现「扣了通用令牌、分片令牌
    /// 不够」这种白扣一枚的情况；非阻塞也是必须的，否则发一块要等 67ms，把入站读取
    /// 也一起挡住了。
    fn try_acquire_pair(first: &mut Self, second: &mut Self) -> bool {
        first.refill_now();
        second.refill_now();

        if first.tokens < 1.0 || second.tokens < 1.0 {
            return false;
        }

        first.tokens -= 1.0;
        second.tokens -= 1.0;

        true
    }

    /// 距离下一枚令牌还有多久（现在已经能取就是 `ZERO`）
    fn wait_duration(&self) -> Duration {
        if self.tokens >= 1.0 {
            return Duration::ZERO;
        }

        Duration::from_secs_f64((1.0 - self.tokens) / self.rate)
    }

    /// 取一枚令牌，取不到就立刻返回 `false`（不等待）。
    ///
    /// 附件分片那条分支必须用非阻塞的取法：`acquire` 会睡到下一枚令牌补充出来，一次
    /// 传输里每几毫秒睡一次，把 `live` 的入站读取一起挡住。
    fn try_acquire(&mut self) -> bool {
        self.refill_now();

        if self.tokens < 1.0 {
            return false;
        }

        self.tokens -= 1.0;

        true
    }

    /// 换一套额度（R20）：中继在 `server.welcome` 里广告了自己的限流上限时按它重设。
    ///
    /// 先把当前令牌补满再夹到新容量以内，这样缩容之后不会留下「超过容量」的怪状态，
    /// 一次离线补发也不会因为这次调整被拉爆。
    fn retune(&mut self, rate: f64, burst: f64) {
        self.refill_now();
        self.rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            self.rate
        };
        self.burst = if burst.is_finite() && burst >= 1.0 {
            burst
        } else {
            self.burst
        };
        self.tokens = self.tokens.min(self.burst);
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
                    Outcome::Stopped => {
                        abort_transfers(&manager, &mut state);

                        break;
                    }
                    Outcome::Lost(failure) => {
                        // §43：V1 不做断点续传，连接一断就收尾（删掉 .part 并标记失败）
                        abort_transfers(&manager, &mut state);

                        failure
                    }
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
            // 腿已经随 `live` 返回被 Drop 掉了，而退避（最长 30 秒）+ 连接与 welcome
            // 超时（15 + 10 秒）里不会再有任何 P2P 事件：不复位就会「重连中」+「已直连」
            status.p2p = P2pState::Off;
            status.pet_state_hz = DEFAULT_PET_STATE_HZ;
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
                            Ok(Some(dropped)) => {
                                manager.emit_error(
                                    generation,
                                    "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                                );
                                retry_dropped_chat(&manager, &mut state, &dropped);
                            }
                            Ok(None) => {}
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
                    Some(Command::StartTransfer(request)) => {
                        // 没连着也先把 offer 排进队列，重连后随第一次 flush 发出去
                        // 退避期间**没有腿**（`live` 一返回就把 `P2pLink` Drop 了），所以这一单
                        // 只能按中继钉（§8 Phase 10：分片大小必须在 offer 之前定下来）。
                        if let Err(error) =
                            start_outgoing_transfer(&manager, &mut state, *request, link::Route::Relay)
                        {
                            manager.emit_error(generation, error);
                        }
                    }
                    Some(Command::AcceptTransfer { .. })
                    | Some(Command::RejectTransfer { .. })
                    | Some(Command::CancelTransfer { .. }) => {
                        // 退避期间没有会话可操作：offer 还没收到，或者传输已经随连接结束被收尾
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

/// 等中继握手后的第一帧：`server.welcome`（CF 版与自建版都保证它最先发）。
///
/// R20：在拿到它之前**不能补发任何东西**——那时还不知道中继广告的额度，用缺省的
/// 20/20 先冲一批就可能撞穿一个更小的自建额度、被 `close 1008` 踢掉。
async fn read_welcome<S, E>(
    stream: &mut S,
    manager: &Arc<PairManager>,
    generation: u64,
) -> Result<RelayConfig, PairFailure>
where
    S: futures_util::Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    let deadline = tokio::time::Instant::now() + WELCOME_TIMEOUT;

    loop {
        let message = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                return Err(PairFailure {
                    message: format!("连接错误: {error}"),
                    fatal: false,
                });
            }
            Ok(None) => return Err(describe_close(None)),
            Err(_) => {
                return Err(PairFailure {
                    message: "等待中继握手超时".to_string(),
                    fatal: false,
                });
            }
        };

        match message {
            Message::Text(text) => {
                if let Some(config) = handle_server_frame(manager, generation, text.as_str())? {
                    return Ok(config);
                }
            }
            Message::Close(frame) => {
                return Err(describe_close(frame.map(|frame| frame.code.into())));
            }
            // welcome 之前不该有别的帧；真收到就忽略（不丢数据，主循环接着处理后面的）
            _ => {}
        }
    }
}

/// 一条会话的主循环。
///
/// 传输层是泛型的：只要「能收发 [`Message`]」就能接进来，不要求是 WebSocket。
/// 中继那条路直接把 [`client::PairSocket`]（`WebSocketStream`）传进来——它本来就
/// 同时实现了 `Sink<Message>` 与 `Stream<Item = Result<Message, _>>`，所以不需要包装。
///
/// 注意 `T` **不是**「当前生效的传输」，而是这条会话的**读侧来源**：函数一进来就先
/// `read_welcome`（等中继的 `server.welcome`，10 秒超时），而 DataChannel 上永远没有
/// 这一帧，所以 DC 不可能单独充当 `T`。按 §4.2，DC 在 Phase 8b/8c 里只做出站 sink，
/// 外加一条额外的入站分支；信令与 `server.peer` 始终留在中继这条流上。
///
/// 传输专有的处理点，都在下面的 `match` 里，由传输自己翻译：主动关闭发
/// `Message::Close`、入站 `Message::Close` 翻成 `describe_close`、流结束
/// （`stream.next()` 返回 `None`）也按 `describe_close(None)` 收尾、其余控制帧
/// 落到 `Ok(_)` 分支忽略——**入站 `Message::Ping` 也走这一支**（tungstenite 0.30
/// 会把入站 Ping 交给调用方，Pong 才是它内部排队的）。不在这个 `match` 里的还有两处：
/// 上面 `read_welcome` 里那套一模一样的（`Message::Text` / 入站 `Message::Close` /
/// 流结束），以及 ticker 发的 WS `Message::Ping`。ticker 那处**保持不动**——R28 推翻了
/// R21 的初版写法：中继自己会回 Pong（`server-relay/src/relay.rs:322-324`），与对端
/// 在线与否无关；换成对端回 pong 会让「对端离线」变成「心跳超时」。DC 那条腿有自己
/// 独立的探针（应用级 `pair.ping`，8b 已落地），两条腿的标志互不共用。
async fn live<T, E>(
    manager: &Arc<PairManager>,
    generation: u64,
    state: &mut SessionState,
    transport: T,
    receiver: &mut mpsc::UnboundedReceiver<Command>,
) -> Outcome
where
    T: futures_util::Sink<Message, Error = E>
        + futures_util::Stream<Item = Result<Message, E>>
        + Unpin,
    E: std::fmt::Display,
{
    let (mut sink, mut stream) = transport.split();
    let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
    // 附件分片另有一层额度（R18）：两套都放行才发一块
    let mut chunk_pacer = Pacer::new(OUTBOUND_CHUNKS_PER_SECOND, OUTBOUND_CHUNK_BURST);
    // DC 那条腿自己的额度（R23）。它**既不吃 `pacer` 也不吃 `chunk_pacer`**：DC 上的帧
    // 根本不经过中继的计费点，拿中继额度去压它正好会把可覆盖流压到 20 帧/秒，
    // 与 60Hz 的目标冲突。所以这条腿的预算就是 §6 的上限本身。
    let mut direct_pacer = Pacer::new(DIRECT_FRAMES_PER_SECOND, DIRECT_BURST);
    // DC 上的**附件分片**再单独一套（R23 / R32）：48 KiB 的块绝不能不限量地塞进
    // 那条既跑聊天又跑控制的通道，也不能去蹭 60Hz 那个桶（会把快照饿死）。
    let mut direct_chunk_pacer = Pacer::new(DIRECT_CHUNKS_PER_SECOND, DIRECT_CHUNK_BURST);

    // R20：先读掉 `server.welcome` 再开始补发（额度与 ICE 广告都在它里面）
    let config = match read_welcome(&mut stream, manager, generation).await {
        Ok(config) => config,
        Err(error) => return Outcome::Lost(error),
    };
    let outbound = config.limits.outbound();

    pacer.retune(outbound.frames_per_second, outbound.frames_burst);
    chunk_pacer.retune(outbound.chunks_per_second, outbound.chunks_burst);

    // 退避期间攒下的聊天与附件控制帧还在这里，但那时腿还不存在，只能按中继发（`None`）
    if let Err(error) = flush(&mut sink, state, &mut pacer, None, false).await {
        return Outcome::Lost(PairFailure {
            message: error,
            fatal: false,
        });
    }

    // 退避期间攒下的可覆盖帧在这里兜底：那时腿还不存在，所以按中继发（`None`）
    if let Err(error) =
        flush_replaceable(&mut sink, None, state, &mut pacer, &mut direct_pacer).await
    {
        return Outcome::Lost(PairFailure {
            message: error,
            fatal: false,
        });
    }

    let heartbeat = heartbeat_interval();
    let mut ticker = tokio::time::interval(heartbeat);

    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // R21：P2P 那条腿。能力门控只看对端——腿一起来就先发 hello，只有收到对端的 hello
    // 才会开始 ICE。信令永远钉在中继上，所以这条腿只做出站 sink 与一条额外的入站分支。
    let (link, link_events) =
        link::P2pLink::spawn(manager.device_id().to_string(), config.ice_servers);
    // `None` = 这条腿结束了，那一支要停掉（`recv()` 会立刻返回 `None`，否则是忙循环）
    let mut link_events = Some(link_events);

    // 每次（重）连都从 `Off` 开始：上一轮留下的 `Connected` 在腿重新协商成功之前都是假的
    publish_route(
        manager,
        generation,
        P2pState::Off,
        false,
        outbound.frames_per_second,
    );

    // 中继腿的探针：永远是 WS Ping，中继自己回 Pong（R28）。它只由中继入站清除。
    let mut relay_awaiting_pong = false;
    let mut relay_last_inbound = tokio::time::Instant::now();

    // DC 腿的探针：应用级 `pair.ping`，**独立标志**（R28）。共用一个标志会造出
    // 「中继静默半死、却被 DC 的 pet-state 流量掩盖」的死角。
    let mut dc_open = false;
    // R30：`dc_open` 只说明 SCTP 协商完了，`dc_verified` 才是「这条腿真的过过数据」的
    // 证据（DC 入站置位）。可覆盖流要两个都满足才切过去——只认 `dc_open` 会把快照灌进
    // 一条 open 但打不通的通道，对端猫冻住整整一个探针超时（默认两分钟），而两边 UI
    // 都写着「已直连」。
    let mut dc_verified = false;
    let mut dc_awaiting_pong = false;
    let mut dc_last_inbound = tokio::time::Instant::now();

    // 可靠那条腿（`reliable` 通道）的探针与标志（Phase 10 / R32）。与 `dc_*` 同样是
    // **独立的一套**：那条通道承载聊天与附件分片，可用性判断错一次就会把附件判死
    // （`direct_lost`），不能拿可覆盖腿的探针结果替它背书。
    let mut reliable_open = false;
    let mut reliable_verified = false;
    let mut reliable_awaiting_pong = false;
    let mut reliable_last_inbound = tokio::time::Instant::now();

    loop {
        // 附件分片走单独一条分支：每次最多发一块，且必须拿到 pacing 令牌（R18）。
        // 这样一次几百块的传输不会像补发队列那样长时间挡住入站读取。
        let chunk_wait = if state.has_pending_chunks() {
            chunk_wait_duration(
                state.next_sending_route(),
                reliable_leg(reliable_open, reliable_verified, &link),
                &pacer,
                &chunk_pacer,
                &direct_chunk_pacer,
            )
        } else {
            None
        };

        tokio::select! {
            _ = async move {
                match chunk_wait {
                    Some(wait) => tokio::time::sleep(wait).await,
                    None => std::future::pending().await,
                }
            } => {
                match send_next_chunk(
                    &mut sink,
                    manager,
                    state,
                    &mut pacer,
                    &mut chunk_pacer,
                    &mut direct_chunk_pacer,
                    reliable_leg(reliable_open, reliable_verified, &link),
                )
                .await
                {
                    Ok(_) => {}
                    Err(error) => return Outcome::Lost(PairFailure { message: error, fatal: false }),
                }
            },
            command = receiver.recv() => match command {
                None | Some(Command::Disconnect) => {
                    let _ = tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Close(None))).await;

                    return Outcome::Stopped;
                }
                Some(Command::Send { kind, envelope }) => {
                    match state.queue(kind, &envelope, false) {
                        Err(error) => manager.emit_error(generation, error),
                        Ok(dropped) => {
                            if let Some(dropped) = dropped {
                                manager.emit_error(
                                    generation,
                                    "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                                );
                                retry_dropped_chat(manager, state, &dropped);
                            }

                            // 聊天（与它的 ack）在可靠腿可用时走 DC（Phase 10）：这条腿有
                            // DB 的补发兜底，丢了不会进终态。
                            if let Err(error) = flush(
                                &mut sink,
                                state,
                                &mut pacer,
                                reliable_leg(reliable_open, reliable_verified, &link),
                                false,
                            )
                            .await
                            {
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

                    // 顺序与拆分前一致：先可靠队列、后可覆盖队列
                    if let Err(error) = flush(
                        &mut sink,
                        state,
                        &mut pacer,
                        reliable_leg(reliable_open, reliable_verified, &link),
                        false,
                    )
                    .await
                    {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }

                    let leg = coverable_leg(dc_open, dc_verified, &link);

                    if let Err(error) = flush_replaceable(
                        &mut sink,
                        leg,
                        state,
                        &mut pacer,
                        &mut direct_pacer,
                    )
                    .await
                    {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }
                }
                Some(Command::StartTransfer(request)) => {
                    // 这一单钉在哪条腿上，在 offer 之前就定下来（§8 Phase 10）：分片大小要
                    // 跟着它走，而且这一单的后半程不能换腿。
                    let leg = reliable_leg(reliable_open, reliable_verified, &link);
                    let route = if leg.is_some() {
                        link::Route::Direct
                    } else {
                        link::Route::Relay
                    };

                    if let Err(error) = start_outgoing_transfer(manager, state, *request, route) {
                        manager.emit_error(generation, error);
                    } else if let Err(error) = flush(
                        &mut sink,
                        state,
                        &mut pacer,
                        leg,
                        // offer 也要和这一单走同一条腿：接收侧正是按 offer 来的 lane 钉 route
                        route == link::Route::Direct,
                    )
                    .await
                    {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }
                }
                Some(Command::AcceptTransfer { transfer_id }) => {
                    // 回执按**这一单钉的 route** 选腿（见 `reply_leg`）：钉在中继上的一单，
                    // 它的 accept 绝不能走 DC——DC 上丢了只有 `direct_lost()` 收尾，而它只管
                    // Direct 的会话，发送方会永远停在 `AwaitingAccept`。
                    let leg = reply_leg(
                        transfer_route(state, transfer_id),
                        reliable_open,
                        reliable_verified,
                        &link,
                    );

                    match accept_incoming_transfer(manager, state, transfer_id) {
                        Ok(Some((kind, reply))) => {
                            if let Err(error) = enqueue_reply(
                                &mut sink,
                                manager,
                                state,
                                &mut pacer,
                                leg,
                                generation,
                                kind,
                                reply,
                            )
                            .await
                            {
                                return Outcome::Lost(PairFailure { message: error, fatal: false });
                            }
                        }
                        Ok(None) => {}
                        Err(error) => manager.emit_error(generation, error),
                    }
                }
                Some(Command::RejectTransfer { transfer_id }) => {
                    // `close_transfer` 会把会话摘掉，所以 route 必须先读
                    let leg = reply_leg(
                        transfer_route(state, transfer_id),
                        reliable_open,
                        reliable_verified,
                        &link,
                    );
                    let reply = AppEnvelope::new(
                        message_type::TRANSFER_REJECT,
                        manager.next_envelope_seq(),
                        json!(TransferRejectPayload {
                            transfer_id,
                            reason: "你拒绝了这次传输".to_string(),
                        }),
                    );

                    close_transfer(
                        manager,
                        state,
                        transfer_id,
                        TransferOutcome::Canceled,
                        "你拒绝了这次传输",
                    );

                    if let Err(error) = enqueue_reply(
                        &mut sink,
                        manager,
                        state,
                        &mut pacer,
                        leg,
                        generation,
                        FrameKind::TransferControl,
                        reply,
                    )
                    .await
                    {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }
                }
                Some(Command::CancelTransfer { transfer_id }) => {
                    // 同上：会话马上就被 `close_transfer` 摘掉，route 先读
                    let leg = reply_leg(
                        transfer_route(state, transfer_id),
                        reliable_open,
                        reliable_verified,
                        &link,
                    );
                    let reply = AppEnvelope::new(
                        message_type::TRANSFER_CANCEL,
                        manager.next_envelope_seq(),
                        json!(TransferIdPayload { transfer_id }),
                    );

                    close_transfer(
                        manager,
                        state,
                        transfer_id,
                        TransferOutcome::Canceled,
                        "你取消了这次传输",
                    );

                    if let Err(error) = enqueue_reply(
                        &mut sink,
                        manager,
                        state,
                        &mut pacer,
                        leg,
                        generation,
                        FrameKind::TransferControl,
                        reply,
                    )
                    .await
                    {
                        return Outcome::Lost(PairFailure { message: error, fatal: false });
                    }
                }
            },
            incoming = stream.next() => {
                let Some(incoming) = incoming else {
                    return Outcome::Lost(describe_close(None));
                };

                relay_last_inbound = tokio::time::Instant::now();
                relay_awaiting_pong = false;

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

                        // 中继来的帧没有 lane（`None`）；回执也照原路回中继——中继腿的探针
                        // 结果绝不能让 DC 那条腿替它背书（R28），所以 pong 必须走中继。
                        match handle_binary(manager, generation, state, &bytes, Some(&link), None) {
                            Err(error) => manager.emit_error(generation, error),
                            Ok(Some((kind, reply))) => {
                                match state.queue(kind, &reply, false) {
                                    Err(error) => manager.emit_error(generation, error),
                                    Ok(dropped) => {
                                        if let Some(dropped) = dropped {
                                            manager.emit_error(
                                                generation,
                                                "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                                            );
                                            retry_dropped_chat(manager, state, &dropped);
                                        }

                                        if let Err(error) =
                                            flush(&mut sink, state, &mut pacer, None, false).await
                                        {
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
                        match handle_server_frame(manager, generation, text.as_str()) {
                            Err(error) => return Outcome::Lost(error),
                            // R20：正常路径上 welcome 已经被 `read_welcome` 读掉并应用过；
                            // 这里兜住「中继再广告一次额度」的情况（重设是幂等的）
                            Ok(Some(config)) => {
                                let outbound = config.limits.outbound();

                                pacer.retune(outbound.frames_per_second, outbound.frames_burst);
                                chunk_pacer.retune(
                                    outbound.chunks_per_second,
                                    outbound.chunks_burst,
                                );

                                // 额度变了，快照上限可能跟着变（R23）。这里只重算它，
                                // `p2p` 由腿自己的事件负责，别在这一支里动。
                                let hz = pet_state_hz(dc_open && dc_verified, outbound.frames_per_second);

                                manager.publish(generation, |status| status.pet_state_hz = hz);

                                // `iceServers` 只在会话建立时读一次：协商用的 STUN/TURN 中途
                                // 换掉会让两侧的候选对不上，要换得等下一轮协商
                            }
                            Ok(None) => {}
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
                // 中继腿的探针**保持不动**：WS Ping 由中继自己回 Pong，与对端在线与否
                // 无关（R28 推翻了 R21 的初版写法）。超时就是整条会话的失败。
                if relay_awaiting_pong && relay_last_inbound.elapsed() >= heartbeat * 2 {
                    return Outcome::Lost(PairFailure {
                        message: "心跳超时".into(),
                        fatal: false,
                    });
                }

                relay_awaiting_pong = true;

                if let Err(error) = send_frame(&mut sink, Message::Ping(Vec::new().into())).await {
                    return Outcome::Lost(PairFailure {
                        message: error,
                        fatal: false,
                    });
                }

                // DC 腿的探针：应用级 `pair.ping`、**独立标志**（R28）。它**不过中继的
                // pacer**——单一 pacer 会把 DC 上的可覆盖流压到中继的 20 帧/秒，和 60Hz
                // 的目标直接冲突（R23）。
                if dc_open {
                    if dc_awaiting_pong && dc_last_inbound.elapsed() >= heartbeat * 2 {
                        // 只把这条腿判为不可用（R30：可覆盖流在这里回落中继）。**绝不返回
                        // `Outcome::Lost`**：那会让 `run_session` 重连整条会话并
                        // `abort_transfers`，砍掉正在传的附件（R21 修正 3）。
                        dc_open = false;
                        dc_verified = false;
                        publish_route(
                            manager,
                            generation,
                            P2pState::Connecting,
                            false,
                            outbound.frames_per_second,
                        );
                    } else {
                        match build_frame(manager, state, FrameKind::Ping, message_type::PING, json!({}))
                        {
                            Ok(frame) => {
                                dc_awaiting_pong = true;
                                link.send(link::Lane::Replaceable, frame);
                            }
                            Err(error) => manager.emit_error(generation, error),
                        }
                    }
                }

                // 可靠那条腿的探针（Phase 10 / R32）。和可覆盖腿一样**独立**，理由也一样：
                // 这条腿承载聊天与在传的附件，探针漏判一次就会把附件整单判死。
                if reliable_open {
                    if reliable_awaiting_pong && reliable_last_inbound.elapsed() >= heartbeat * 2
                    {
                        reliable_open = false;
                        reliable_verified = false;
                        reliable_awaiting_pong = false;

                        // 半死的腿必须**两处**一起收尾（另一处是 `ChannelClosed(Reliable)`）：
                        // 只挂「关闭」会让这一单永远留在表里，V1 没有任何停滞超时。
                        if let Err(error) =
                            direct_lost(&mut sink, manager, generation, state, &mut pacer).await
                        {
                            return Outcome::Lost(PairFailure {
                                message: error,
                                fatal: false,
                            });
                        }

                        // 这条腿上发出、还没等到 ack 的聊天退回「等待发送」（§32）
                        manager.resend_pending_chat();
                    } else {
                        match build_frame(manager, state, FrameKind::Ping, message_type::PING, json!({}))
                        {
                            Ok(frame) => {
                                reliable_awaiting_pong = true;
                                link.send(link::Lane::Reliable, frame);
                            }
                            Err(error) => manager.emit_error(generation, error),
                        }
                    }
                }
            },
            event = async {
                match link_events.as_mut() {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    // 这条腿结束了（驱动循环只在会话收摊时退出）。停掉这一支——通道关闭
                    // 之后 `next()` 会立刻返回 `None`，不处理就是忙循环。
                    None => link_events = None,
                    Some(link::P2pEvent::Signal(signal)) => {
                        // 信令走 kind 8（R21）。**不进 `state.reliable`**：那条队列上限 512，
                        // 队满时会挤掉最旧的聊天消息并把它退回 `pending`，而信令一轮只有
                        // 个位数帧，没必要让聊天替它承担这个风险。
                        if let Ok(payload) = serde_json::to_value(&signal) {
                            match build_frame(
                                manager,
                                state,
                                FrameKind::Ping,
                                message_type::SIGNAL,
                                payload,
                            ) {
                                Ok(frame) => {
                                    pacer.acquire().await;

                                    if let Err(error) =
                                        send_frame(&mut sink, Message::Binary(frame.into())).await
                                    {
                                        return Outcome::Lost(PairFailure {
                                            message: error,
                                            fatal: false,
                                        });
                                    }
                                }
                                Err(error) => manager.emit_error(generation, error),
                            }
                        }
                    }
                    Some(link::P2pEvent::Negotiating) => {
                        publish_route(
                            manager,
                            generation,
                            P2pState::Connecting,
                            false,
                            outbound.frames_per_second,
                        );
                    }
                    Some(link::P2pEvent::ChannelOpen(lane)) => match lane {
                        link::Lane::Replaceable => {
                            dc_open = true;
                            // 刚 open 的通道还没过过任何数据，先不当它可用（R30）
                            dc_verified = false;
                            dc_awaiting_pong = false;
                            dc_last_inbound = tokio::time::Instant::now();

                            // 立刻验一次：等一个 tick（默认 60 秒）才验到的话，可覆盖流会白等
                            // 一分钟才切过去。`dc_verified` 由 DC 入站置位。
                            match build_frame(manager, state, FrameKind::Ping, message_type::PING, json!({}))
                            {
                                Ok(frame) => {
                                    dc_awaiting_pong = true;
                                    link.send(link::Lane::Replaceable, frame);
                                }
                                Err(error) => manager.emit_error(generation, error),
                            }

                            publish_route(
                                manager,
                                generation,
                                P2pState::Connecting,
                                false,
                                outbound.frames_per_second,
                            );
                        }
                        link::Lane::Reliable => {
                            reliable_open = true;
                            reliable_verified = false;
                            reliable_awaiting_pong = false;
                            reliable_last_inbound = tokio::time::Instant::now();

                            // 同样立刻验一次：`reliable_verified` 由这条 lane 的入站置位
                            match build_frame(manager, state, FrameKind::Ping, message_type::PING, json!({}))
                            {
                                Ok(frame) => {
                                    reliable_awaiting_pong = true;
                                    link.send(link::Lane::Reliable, frame);
                                }
                                Err(error) => manager.emit_error(generation, error),
                            }

                            // UI 的 `p2p` 只描述可覆盖腿（§8 Phase 10）：可靠腿起来不改它
                        }
                    },
                    Some(link::P2pEvent::ChannelClosed(lane)) => match lane {
                        link::Lane::Replaceable => {
                            dc_open = false;
                            dc_verified = false;

                            // 腿会自己重试，所以是「正在协商」而不是「关闭」
                            publish_route(
                                manager,
                                generation,
                                P2pState::Connecting,
                                false,
                                outbound.frames_per_second,
                            );
                        }
                        link::Lane::Reliable => {
                            reliable_open = false;
                            reliable_verified = false;
                            reliable_awaiting_pong = false;

                            // 钉在这条腿上的附件全部收尾（§43）：通道没了就再也收不到分片了
                            if let Err(error) =
                                direct_lost(&mut sink, manager, generation, state, &mut pacer).await
                            {
                                return Outcome::Lost(PairFailure {
                                    message: error,
                                    fatal: false,
                                });
                            }

                            manager.resend_pending_chat();
                        }
                    },
                    Some(link::P2pEvent::Inbound(lane, bytes)) => {
                        // DC 腿的入站（§4.2 / R28）：只清 DC 腿**自己的**标志。**不要**动
                        // 中继腿的 `relay_awaiting_pong` / `relay_last_inbound`——让 DC 的流量去清中继
                        // 腿的标志，正是「中继静默半死被掩盖」的成因。
                        //
                        // R32：两条 lane 的「真的过过数据」证据**各自独立**。`pet-state` 上
                        // 收到的东西不能替 `reliable` 背书（那条腿承载在传的附件，判错一次
                        // 就是整单报废），反之亦然。
                        match lane {
                            link::Lane::Replaceable => {
                                dc_last_inbound = tokio::time::Instant::now();
                                dc_awaiting_pong = false;

                                // R30：**任何** DC 入站都算一次成功的往返（探针的 pong 只是其中
                                // 一种），这是「这条腿真的能过数据」的唯一证据。只有它才允许可
                                // 覆盖流切过去。
                                //
                                // 发布也要卡在 `dc_open` 上：探针超时只把腿判为不可用、并没有关
                                // 掉通道（超时 ≠ 关闭），所以超时之后对端恢复的流量照样会进来；
                                // 不卡的话 UI 会在选路已经回到中继的情况下又亮起「已直连」，而且
                                // 不会再自动复位。
                                if dc_open && !dc_verified {
                                    dc_verified = true;
                                    publish_route(
                                        manager,
                                        generation,
                                        P2pState::Connected,
                                        true,
                                        outbound.frames_per_second,
                                    );
                                }
                            }
                            link::Lane::Reliable => {
                                reliable_last_inbound = tokio::time::Instant::now();
                                reliable_awaiting_pong = false;

                                if reliable_open {
                                    reliable_verified = true;
                                }
                            }
                        }

                        if bytes.len() > MAX_BINARY_FRAME_SIZE {
                            // 超限只丢这一帧：DC 是我们自己的通道，不必像中继那样断线
                            manager.emit_error(generation, "收到超过上限的帧".into());
                        } else {
                            match handle_binary(
                                manager,
                                generation,
                                state,
                                &bytes,
                                Some(&link),
                                Some(lane),
                            ) {
                                Err(error) => manager.emit_error(generation, error),
                                Ok(Some((kind, reply))) => {
                                    // 请求从 DC 来、回复也从 DC 回去，否则探针的 pong 会绕
                                    // 中继，「DC 腿的探针」就名不副实了；两条 lane 同理，各回
                                    // 各的（`reliable` 上的 offer / 分片回执绝不能绕中继）
                                    if let Ok(frame) = state.encode(kind, &reply) {
                                        link.send(lane, frame);
                                    }
                                }
                                Ok(None) => {}
                            }
                        }
                    }
                }
            },
        }
    }
}

/// 前端该按多少 Hz 发桌宠快照（§6 / R23）。
///
/// 只看**当前生效传输**的额度：DC 那条腿没有中继的令牌桶，直接用 60Hz 上限；中继腿用
/// `server.welcome` 广告值推导出的帧额度，够 60 才提到上限（自建中继广告 90 帧/秒 →
/// 推导 60，§10 的那条验收就是它）。两条都不满足就退回 v1 的 3Hz —— CF 缺省 30 →
/// 推导 20，所以「CF 版行为一字不变」。
///
/// `direct` 指的是**桌宠快照真正要走的**那条腿（DC 的 `pet-state` 通道），不是 DC 上的
/// `reliable` 通道：两者的可用性各自独立，后者只影响聊天与附件走哪条腿（Phase 10）。
fn pet_state_hz(direct: bool, relay_frames_per_second: f64) -> f64 {
    if direct || relay_frames_per_second >= MAX_PET_STATE_HZ {
        MAX_PET_STATE_HZ
    } else {
        DEFAULT_PET_STATE_HZ
    }
}

/// 一起发布「当前生效传输」派生出来的两个字段（§6 / R23）。
///
/// 它们由同一对标志决定，所以必须同一次写完：分开写会造出「已直连但仍然按 3Hz 发」
/// 或者反过来的中间态，而前端是各读各的。
fn publish_route(
    manager: &Arc<PairManager>,
    generation: u64,
    p2p: P2pState,
    direct: bool,
    relay_frames_per_second: f64,
) {
    let hz = pet_state_hz(direct, relay_frames_per_second);

    manager.publish(generation, |status| {
        status.p2p = p2p;
        status.pet_state_hz = hz;
    });
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

/// 排**可靠**队列（聊天、presence、控制、附件分片）。
///
/// 默认走中继。`leg = Some` 时走 DC 的 `reliable` 通道（Phase 10 / R32）——那是**有序
/// 且可靠**的一条通道，所以分片可以走它；`pet-state` 那条（`ordered = false,
/// max_retransmits = 0`）永远不承载分片（R21）。
///
/// 走 DC 时**不占中继的 `pacer`**：那条通道不经过中继的计费点（R23）。这里也不给它
/// 单独设桶——可靠帧小且稀，而 `writable` 标志 + DC 的发送缓冲上限已经在源头封住了内存。
///
/// **腿的背压翻假就整体回退中继**（R32）：DC 的 `send` 会一直等到缓冲降到低水位才返回，
/// 而本函数在 `live` 的 `select!` 分支里被 await——等下去会把中继腿的入站读取、连心跳
/// 一起挡住。退回中继腿一条帧都不会丢（中继有序，聊天顺序照样对）。
///
/// `force` **绕过背压判断**（`leg` 有就给 `leg`）：只有「这一批帧必须和某个已钉 route 的
/// 传输走同一条 lane」时才用一次——`transfer.complete` 排在那条腿的分片后面，跨 lane 没有
/// 顺序保证（见 `send_next_chunk`）。`leg.send` 只是往那条腿的队列里投一帧、不阻塞，
/// 所以这里绕过的只是「源头拒绝注入」，不是「把 `live` 睡死」。
async fn flush<S>(
    sink: &mut S,
    state: &mut SessionState,
    pacer: &mut Pacer,
    leg: Option<&dyn link::ReliableLeg>,
    force: bool,
) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let leg = if force {
        leg
    } else {
        leg.filter(|leg| leg.writable())
    };

    while let Some((frame, envelope)) = state.reliable.pop_front() {
        if let Some(leg) = leg {
            // 失败即丢：这条腿上的帧丢了靠中继兜底——聊天有 DB 的补发
            // （`resend_pending_chat`），在传的附件由 `direct_lost()` 按 §43 判失败
            leg.send(frame);

            continue;
        }

        pacer.acquire().await;

        if let Err(error) = send_frame(sink, Message::Binary(frame.clone().into())).await {
            state.reliable.push_front((frame, envelope));

            return Err(error);
        }
    }

    Ok(())
}

/// 可覆盖流（宠物快照、统计）能不能走 DC：两个条件都满足才算（R30）。
///
/// `dc_open` 只说明 SCTP 协商完了；`dc_verified` 才是「这条腿真的过过数据」的证据
/// （见 `live` 的 DC 入站分支）。只认 `dc_open` 会把快照灌进一条 open 但打不通的通道：
/// 默认心跳下探针超时是两分钟，这段时间里对端猫是冻住的，而两边 UI 都写着「已直连」。
fn coverable_leg<'a>(
    dc_open: bool,
    dc_verified: bool,
    link: &'a link::P2pLink,
) -> Option<&'a dyn link::CoverableLeg> {
    (dc_open && dc_verified).then_some(link as &dyn link::CoverableLeg)
}

/// 可靠流（聊天、presence、控制、附件分片）能不能走 DC 的第二条通道（R32）。
///
/// 与 `coverable_leg` 同一个形状，但读的是**另一条通道**的两个标志：`reliable_open` 只
/// 说明 SCTP 协商完了，`reliable_verified` 才是「这条通道真的往返过一次」的证据（而且
/// 那次往返必须是**我们发出去的探针被答复**——我们要用的正是出站方向）。
fn reliable_leg<'a>(
    reliable_open: bool,
    reliable_verified: bool,
    link: &'a link::P2pLink,
) -> Option<&'a dyn link::ReliableLeg> {
    (reliable_open && reliable_verified).then_some(link as &dyn link::ReliableLeg)
}

/// 一单的**回执**（accept / reject / cancel）该走哪条腿（R32）。
///
/// **按这一单钉住的 `route` 选，不是按「此刻哪条腿可用」**。走 DC 的帧失败即丢（这是设计），
/// 而「丢了」的收尾只挂在 `direct_lost()` 上——它只管 `route == Direct` 的会话。所以钉在
/// **中继**上的那一单，它的回执绝不能走 DC：万一那枚帧撞上「通道已经关了、但
/// `ChannelClosed(Reliable)` 还没被 `live` 处理」的窗口（`select!` 在多个就绪分支里随机选，
/// 命令那一支完全可能先跑），发送方会永远停在 `AwaitingAccept`（V1 没有任何停滞超时），
/// 而中继那条腿心跳一切正常，两边就这么挂着。
///
/// `Direct` 那一单反过来优先走那条腿（offer 就是从那儿来的）；即便它被背压挡回中继也不
/// 影响正确性——那一侧本来就有 `direct_lost()` 收敛。
fn reply_leg<'a>(
    route: link::Route,
    reliable_open: bool,
    reliable_verified: bool,
    link: &'a link::P2pLink,
) -> Option<&'a dyn link::ReliableLeg> {
    match route {
        link::Route::Direct => reliable_leg(reliable_open, reliable_verified, link),
        link::Route::Relay => None,
    }
}

/// 某一单钉在哪条腿上；会话已经收掉（未知 id）就按中继——回执宁愿绕中继也不能丢。
fn transfer_route(state: &SessionState, transfer_id: u64) -> link::Route {
    state
        .transfers
        .get(&transfer_id)
        .map(|session| session.route)
        .unwrap_or_default()
}

/// 附件分片那条 `select!` 分支的等待时长（R18 / R32）。
///
/// **不变量**：返回 `ZERO` 必须意味着紧接着的 `send_next_chunk` 一定发得出去，否则
/// `select!` 会在 `Ok(false)` 与 `ZERO` 之间空转。所以这里要把那一单真正需要的每一项
/// 都算进来：中继那一单是「通用额度 + 分片额度」，Direct 那一单是「分片额度 + 背压」。
/// 背压没有可计算的剩余时间，翻假时就用 [`DIRECT_RETRY_INTERVAL`] 轮询。
fn chunk_wait_duration(
    route: Option<link::Route>,
    direct: Option<&dyn link::ReliableLeg>,
    pacer: &Pacer,
    chunk_pacer: &Pacer,
    direct_chunk_pacer: &Pacer,
) -> Option<Duration> {
    Some(match route? {
        link::Route::Relay => pacer.wait_duration().max(chunk_pacer.wait_duration()),
        link::Route::Direct => {
            let wait = direct_chunk_pacer.wait_duration();

            if wait == Duration::ZERO && direct.is_some_and(|leg| leg.writable()) {
                Duration::ZERO
            } else {
                wait.max(DIRECT_RETRY_INTERVAL)
            }
        }
    })
}

/// 排**可覆盖**流（宠物快照、统计）：DC 可用时走它，否则照旧走中继。
///
/// 走 DC 时**不占中继的 pacer**，改用那条腿自己的 `direct_pacer`（R23）：单一 pacer 会把
/// DC 上的可覆盖流压到中继的 20 帧/秒，与 60Hz 的目标直接冲突。DC 的预算就是 §6 的
/// 60Hz 上限本身（`DIRECT_FRAMES_PER_SECOND`），与中继广告的额度无关。
///
/// 写 `state.replaceable` 的只有两处（退避期的 `FlushReplaceable`、`live` 里同一个
/// 分支），所以只有这两个地方需要调用本函数；会话开始那次是给退避期攒下的帧兜底。
async fn flush_replaceable<S>(
    sink: &mut S,
    leg: Option<&dyn link::CoverableLeg>,
    state: &mut SessionState,
    pacer: &mut Pacer,
    direct_pacer: &mut Pacer,
) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    while let Some((key, frame)) = state.replaceable.pop_first() {
        if let Some(leg) = leg {
            // DC 那条腿有自己的额度（R23）。这里**不**吃中继的 pacer：DC 上的帧不经过
            // 中继的计费点，用中继额度去压它就会把 60Hz 压回 20 帧/秒。
            direct_pacer.acquire().await;

            // 可覆盖流是绝对值快照：发失败就丢，等下一帧盖掉它
            leg.send(frame);

            continue;
        }

        pacer.acquire().await;

        if let Err(error) = send_frame(sink, Message::Binary(frame.clone().into())).await {
            state.replaceable.insert(key, frame);

            return Err(error);
        }
    }

    Ok(())
}

/// 入站消息产生的回执：放进可靠队列并立刻尝试发出去
async fn enqueue_reply<S>(
    sink: &mut S,
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    pacer: &mut Pacer,
    leg: Option<&dyn link::ReliableLeg>,
    generation: u64,
    kind: FrameKind,
    envelope: AppEnvelope,
) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    match state.queue(kind, &envelope, false) {
        Err(error) => {
            manager.emit_error(generation, error);

            Ok(())
        }
        Ok(dropped) => {
            if let Some(dropped) = dropped {
                manager.emit_error(
                    generation,
                    "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                );
                retry_dropped_chat(manager, state, &dropped);
            }

            flush(sink, state, pacer, leg, false).await
        }
    }
}

/// 队列满时被挤掉的聊天消息退回「等待发送」（§32）：下一次对端上线会重新补发，
/// 既不假装「已发送」，也不会变成无法重试的终态。
fn retry_dropped_chat(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    envelope: &AppEnvelope,
) {
    let Some(id) = envelope.payload.get("messageId").and_then(Value::as_str) else {
        return;
    };

    match envelope.message_type.as_str() {
        message_type::CHAT_TEXT => manager.publish_message_status(id, MessageStatus::Pending),
        // 附件 offer 被挤掉时对方永远等不到分片，只能标记失败让用户重试。会话要一起收掉，
        // 否则这个永远等不到 accept 的会话会一直占着 MAX_ACTIVE_TRANSFERS 的名额
        message_type::TRANSFER_OFFER => {
            let reason = "发送队列已满，附件没有发出去，可以重试";

            match manager.transfer_of(id) {
                Ok(transfer_id) => {
                    close_transfer(manager, state, transfer_id, TransferOutcome::Failed, reason)
                }
                Err(_) => manager.fail_attachment(id, reason),
            }
        }
        _ => {}
    }
}

/// 组装一条进度事件
fn progress_payload(
    session: &TransferSession,
    state: &'static str,
    transferred: u64,
    message: Option<String>,
) -> TransferProgress {
    let percent = if session.size == 0 {
        100
    } else {
        ((transferred as f64 / session.size as f64).clamp(0.0, 1.0) * 100.0).round() as u8
    };

    TransferProgress {
        transfer_id: session.id,
        message_id: session.message_id.clone(),
        attachment_id: session.attachment_id.clone(),
        kind: session.kind,
        name: session.name.clone(),
        size: session.size,
        transferred,
        percent,
        direction: session.direction(),
        state,
        message,
    }
}

/// 进度节流：`force` 用于阶段变化与结束，其余情况每个 transfer 最多 150ms 报一次
fn report_progress(
    manager: &Arc<PairManager>,
    session: &mut TransferSession,
    state: &'static str,
    message: Option<String>,
    force: bool,
) {
    let now = tokio::time::Instant::now();
    let due = session
        .last_progress_at
        .map(|last| now.duration_since(last) >= TRANSFER_PROGRESS_INTERVAL)
        .unwrap_or(true);

    if !force && !due {
        return;
    }

    session.last_progress_at = Some(now);

    let payload = progress_payload(session, state, session.transferred(), message);

    manager.publish_transfer(&payload);
}

/// 一次传输的收尾方式（§43）。取消与失败在 UI 上不是一回事：取消是用户自己的决定，
/// 显示「传输失败」会让人以为出了故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferOutcome {
    Done,
    Failed,
    Canceled,
}

impl TransferOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    /// 取消也算「没送到」：消息标成 failed，用户重新打开窗口后还能点「重试」（§43）
    const fn marks_failed(self) -> bool {
        !matches!(self, Self::Done)
    }

    /// 只有真的出错才报「最近一次错误」：取消是用户自己的决定，不是故障
    const fn reports_error(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// 一次传输的收尾：标记状态、删掉临时文件、把会话从表里摘掉
fn close_transfer(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    transfer_id: u64,
    outcome: TransferOutcome,
    reason: &str,
) {
    let Some(mut session) = state.transfers.remove(&transfer_id) else {
        return;
    };

    manager.unregister_transfer(&session.message_id);

    if let Some(incoming) = session.incoming.take() {
        incoming.abort();

        let _ = manager
            .history
            .set_attachment_path(&session.attachment_id, None);
    }

    if outcome.marks_failed() {
        if outcome.reports_error() {
            manager.fail_attachment(&session.message_id, reason);
        } else {
            manager.mark_attachment_failed(&session.message_id);
        }
    }

    let payload = progress_payload(
        &session,
        outcome.as_str(),
        session.transferred(),
        if outcome.marks_failed() {
            Some(reason.to_string())
        } else {
            None
        },
    );

    manager.publish_transfer(&payload);
}

/// 连接结束时收尾（§43）：V1 不做断点续传，半成品直接丢掉，消息标记失败以便重试
fn abort_transfers(manager: &Arc<PairManager>, state: &mut SessionState) {
    for mut session in state.take_transfers() {
        manager.unregister_transfer(&session.message_id);

        if let Some(incoming) = session.incoming.take() {
            incoming.abort();

            let _ = manager
                .history
                .set_attachment_path(&session.attachment_id, None);
        }

        manager.publish_message_status(&session.message_id, MessageStatus::Failed);

        let payload = progress_payload(
            &session,
            "failed",
            session.transferred(),
            Some("连接断开，传输已中断".to_string()),
        );

        manager.publish_transfer(&payload);
    }
}

/// 可靠那条腿不可用时的收尾（R32）。
///
/// **触发点有两个，必须都接上**：通道真的关闭（`ChannelClosed(Reliable)`）与探针超时。
/// 只挂「关闭」会让半死的腿把 `route == Direct` 的传输会话**永久留在表里**——V1 没有
/// 任何停滞超时，`abort_transfers` 只在整条会话结束时才跑。
///
/// 除了本地判失败，还要在**中继上显式发一条 `transfer.cancel`**：不能假定两端在同一
/// 时刻拿到同一个事件，而半死的腿正是「一端以为还在传、另一端什么都没收到」的形状。
/// 对端收到未知 `transferId` 的 cancel 是 no-op（`close_transfer` 找不到会话就直接返回），
/// 所以重复无害。
async fn direct_lost<S>(
    sink: &mut S,
    manager: &Arc<PairManager>,
    generation: u64,
    state: &mut SessionState,
    pacer: &mut Pacer,
) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let direct: Vec<u64> = state
        .transfers
        .iter()
        .filter(|(_, session)| session.route == link::Route::Direct)
        .map(|(id, _)| *id)
        .collect();

    if direct.is_empty() {
        return Ok(());
    }

    for transfer_id in direct {
        let envelope = AppEnvelope::new(
            message_type::TRANSFER_CANCEL,
            manager.next_envelope_seq(),
            json!(TransferIdPayload { transfer_id }),
        );

        match state.queue(FrameKind::TransferControl, &envelope, false) {
            Ok(Some(dropped)) => {
                manager.emit_error(
                    generation,
                    "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                );
                retry_dropped_chat(manager, state, &dropped);
            }
            Ok(None) => {}
            Err(error) => manager.emit_error(generation, error),
        }

        close_transfer(
            manager,
            state,
            transfer_id,
            TransferOutcome::Failed,
            "P2P 通路断开，传输已中断",
        );
    }

    // cancel 只能走中继腿：这条腿按定义已经不可用了（`None`）
    flush(sink, state, pacer, None, false).await
}

/// 发送方：发起一次附件 offer（附件与消息行已经落库）
fn start_outgoing_transfer(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    request: OutgoingRequest,
    route: link::Route,
) -> Result<(), String> {
    if request.size > manager.max_attachment_size() {
        return Err(format!(
            "附件超过本机上限（{} MB）",
            manager.max_attachment_size() / (1024 * 1024)
        ));
    }

    if state.transfers.len() >= MAX_ACTIVE_TRANSFERS {
        return Err("同时进行的附件传输太多，请等一会儿再发".to_string());
    }

    // 分片大小随传输层走（§7 / R22 / R32）：DC 那条路用 48 KiB，中继那条路仍是 512 KiB。
    // 这个值写进 offer，接收侧按它算每一块的长度，所以**这一单的后半程不能换腿**。
    let chunk_size = match route {
        link::Route::Direct => P2P_CHUNK_SIZE,
        link::Route::Relay => CHUNK_SIZE,
    };

    let outgoing = OutgoingTransfer::with_digest(
        request.path.clone(),
        &request.name,
        &request.mime,
        request.size,
        request.sha256.clone(),
        chunk_size,
    );

    let session = TransferSession {
        id: request.transfer_id,
        message_id: request.message_id.clone(),
        attachment_id: request.attachment_id.clone(),
        kind: request.kind,
        name: outgoing.name.clone(),
        mime: outgoing.mime.clone(),
        size: outgoing.size,
        sha256: outgoing.sha256.clone(),
        chunk_size: outgoing.chunk_size as u32,
        chunks: outgoing.chunks,
        phase: TransferPhase::AwaitingAccept,
        route,
        outgoing: Some(outgoing),
        incoming: None,
        last_progress_at: None,
    };

    let offer = TransferOfferPayload {
        transfer_id: session.id,
        message_id: session.message_id.clone(),
        attachment_id: session.attachment_id.clone(),
        kind: session.kind,
        name: session.name.clone(),
        size: session.size,
        mime: session.mime.clone(),
        sha256: session.sha256.clone(),
        chunk_size: session.chunk_size,
        chunks: session.chunks,
    };

    state.open_transfer(session)?;

    manager.register_transfer(&request.message_id, request.transfer_id);

    let envelope = AppEnvelope::new(
        message_type::TRANSFER_OFFER,
        manager.next_envelope_seq(),
        serde_json::to_value(offer).map_err(|error| format!("序列化附件 offer 失败: {error}"))?,
    );

    match state.queue(FrameKind::TransferControl, &envelope, false) {
        Ok(Some(dropped)) => {
            manager.emit_error(
                manager.generation(),
                "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
            );
            retry_dropped_chat(manager, state, &dropped);
        }
        Ok(None) => {}
        Err(error) => return Err(error),
    }

    manager.publish_message_status(&request.message_id, MessageStatus::Sent);

    if let Some(session) = state.transfers.get_mut(&request.transfer_id) {
        report_progress(manager, session, "waiting", None, true);
    }

    Ok(())
}

/// 接收方：用户点了「接收」（§42），开始收分片
fn accept_incoming_transfer(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    transfer_id: u64,
) -> Result<Reply, String> {
    let Some(session) = state.transfers.get_mut(&transfer_id) else {
        return Ok(None);
    };

    if session.phase != TransferPhase::AwaitingDecision {
        return Ok(None);
    }

    let incoming = IncomingTransfer::create(
        &manager.store().tmp_dir(),
        session.size,
        &session.sha256,
        session.chunks,
        session.chunk_size as usize,
    )?;

    session.incoming = Some(incoming);
    session.phase = TransferPhase::Receiving;

    report_progress(manager, session, "receiving", None, true);

    Ok(Some((
        FrameKind::TransferControl,
        AppEnvelope::new(
            message_type::TRANSFER_ACCEPT,
            manager.next_envelope_seq(),
            json!(TransferIdPayload { transfer_id }),
        ),
    )))
}

/// 接收方：收到一块分片（per-transfer 密钥已经解好）
fn handle_transfer_chunk(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    header: &FrameHeader,
    plaintext: &[u8],
) -> Result<Reply, String> {
    let Some(session) = state.transfers.get_mut(&header.transfer_id) else {
        return Err("收到未知附件传输的分片".to_string());
    };

    if session.phase != TransferPhase::Receiving {
        return Err("附件传输还没进入接收阶段就收到了分片".to_string());
    }

    session
        .incoming
        .as_mut()
        .ok_or_else(|| "接收中的会话缺少接收状态".to_string())?
        .write_chunk(header.seq, plaintext)?;

    let finished = session
        .incoming
        .as_ref()
        .map(|incoming| incoming.received_chunks >= incoming.chunks)
        .unwrap_or(false);

    if !finished {
        report_progress(manager, session, "receiving", None, false);

        return Ok(None);
    }

    finish_incoming_transfer(manager, state, header.transfer_id)
}

/// 接收方：所有分片到齐，校验 SHA-256 并落盘（§41）
fn finish_incoming_transfer(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    transfer_id: u64,
) -> Result<Reply, String> {
    let Some(mut session) = state.transfers.remove(&transfer_id) else {
        return Ok(None);
    };

    manager.unregister_transfer(&session.message_id);

    let Some(incoming) = session.incoming.take() else {
        return Ok(None);
    };

    let target = manager
        .store()
        .attachment_path(&manager.store().attachment_name(&session.name));

    match incoming.finish(target) {
        Ok((path, _size, _sha256)) => {
            let _ = manager
                .history
                .set_attachment_path(&session.attachment_id, Some(&path.to_string_lossy()));
            manager.publish_message_status(&session.message_id, MessageStatus::Received);

            let payload = progress_payload(&session, "done", session.size, None);

            manager.publish_transfer(&payload);

            Ok(Some((
                FrameKind::TransferControl,
                AppEnvelope::new(
                    message_type::TRANSFER_VERIFIED,
                    manager.next_envelope_seq(),
                    json!(TransferVerifiedPayload {
                        transfer_id,
                        ok: true,
                        message: None,
                    }),
                ),
            )))
        }
        Err(error) => {
            let _ = manager
                .history
                .set_attachment_path(&session.attachment_id, None);
            manager.publish_message_status(&session.message_id, MessageStatus::Failed);

            let payload = progress_payload(&session, "failed", session.transferred(), Some(error.clone()));

            manager.publish_transfer(&payload);

            Ok(Some((
                FrameKind::TransferControl,
                AppEnvelope::new(
                    message_type::TRANSFER_VERIFIED,
                    manager.next_envelope_seq(),
                    json!(TransferVerifiedPayload {
                        transfer_id,
                        ok: false,
                        message: Some(error),
                    }),
                ),
            )))
        }
    }
}

/// 接收方：收到 offer（§39）。大文件先问用户，其余直接开始收。
fn handle_transfer_offer(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    payload: TransferOfferPayload,
    lane: Option<link::Lane>,
) -> Result<Reply, String> {
    let reject = |reason: &str| {
        Ok(Some((
            FrameKind::TransferControl,
            AppEnvelope::new(
                message_type::TRANSFER_REJECT,
                manager.next_envelope_seq(),
                json!(TransferRejectPayload {
                    transfer_id: payload.transfer_id,
                    reason: reason.to_string(),
                }),
            ),
        )))
    };

    // `chunk_size` 是**对端给的**（§7 / R32）：中继 512 KiB、P2P 48 KiB，两种都合法，所以
    // 不能只认本机常量，但也不能照单全收——超范围 / 和 `size` 对不上就**回一条 reject**，
    // **不做夹紧**（夹紧会让两侧用不同的分块长度，每一块都被判「大小不对」，比早失败更难查）。
    //
    // 这里回 reject 而不是像以前那样只在本机 `emit_error`：本地报错的话发送方会一直停在
    // `AwaitingAccept` 等一个永远不来的回执（V1 没有超时），回一条 reject 让它立刻按 §43 收尾。
    // 对未知 `transferId` 的 reject 在对端是 no-op，所以重复回也无害。
    let chunk_size = payload.chunk_size as usize;

    if !chunk_size_is_valid(payload.chunk_size as u64)
        || payload.chunks != chunk_count(payload.size, chunk_size)
        || payload.sha256.len() != 64
        || !payload
            .sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return reject("附件 offer 的参数不合法");
    }

    // 这一单钉在哪条腿上（§8 Phase 10）：offer 从哪条 lane 来就从哪条 lane 走。DC 的
    // `reliable` 能把 offer 送过来，本身就证明那条腿通（与入站数据同一条路），不必再问标志。
    let route = match lane {
        Some(link::Lane::Reliable) => link::Route::Direct,
        _ => link::Route::Relay,
    };

    if state.transfers.contains_key(&payload.transfer_id) {
        return Ok(None);
    }

    if state.transfers.len() >= MAX_ACTIVE_TRANSFERS {
        return reject("对方同时进行的附件传输太多");
    }

    if payload.size > manager.max_attachment_size() {
        return reject("超过本机允许的附件大小");
    }

    let name = sanitize_file_name(&payload.name);
    let mime = sanitize_mime(&payload.mime);
    let kind = match payload.kind {
        TransferKind::Image => MessageKind::Image,
        TransferKind::File => MessageKind::File,
        TransferKind::Voice => MessageKind::Voice,
    };
    let created_at = now_millis();

    manager.history.upsert_attachment(&NewAttachment {
        id: payload.attachment_id.clone(),
        kind,
        original_name: Some(name.clone()),
        mime: Some(mime.clone()),
        size: Some(payload.size),
        sha256: Some(payload.sha256.clone()),
        local_path: None,
        created_at,
    })?;

    // 同一个 messageId 又来了：这是对方在重发，改回 pending 而不是插一条新的
    let existing = manager.history.find(&payload.message_id)?;
    let message = match existing {
        Some(message) if message.direction == MessageDirection::Incoming => {
            manager
                .history
                .set_status(&payload.message_id, MessageStatus::Pending)?
                .unwrap_or(message)
        }
        _ => manager.history.insert(&NewMessage::incoming_attachment(
            payload.message_id.clone(),
            kind,
            payload.attachment_id.clone(),
            created_at,
            manager.history.epoch()?,
        ))?,
    };

    manager.sink.emit(
        EVENT_MESSAGE_RECEIVED,
        serde_json::to_value(&message).unwrap_or(Value::Null),
    );

    let awaiting = needs_confirmation(payload.kind, payload.size);
    let mut session = TransferSession {
        id: payload.transfer_id,
        message_id: payload.message_id,
        attachment_id: payload.attachment_id,
        kind: payload.kind,
        name,
        mime,
        size: payload.size,
        sha256: payload.sha256,
        chunk_size: payload.chunk_size,
        chunks: payload.chunks,
        phase: if awaiting {
            TransferPhase::AwaitingDecision
        } else {
            TransferPhase::Receiving
        },
        route,
        outgoing: None,
        incoming: None,
        last_progress_at: None,
    };

    if !awaiting {
        session.incoming = Some(IncomingTransfer::create(
            &manager.store().tmp_dir(),
            session.size,
            &session.sha256,
            session.chunks,
            session.chunk_size as usize,
        )?);
    }

    let transfer_id = session.id;
    let message_id = session.message_id.clone();

    state.open_transfer(session)?;

    manager.register_transfer(&message_id, transfer_id);

    if let Some(session) = state.transfers.get_mut(&transfer_id) {
        report_progress(
            manager,
            session,
            if awaiting { "waiting" } else { "receiving" },
            None,
            true,
        );
    }

    if awaiting {
        return Ok(None);
    }

    Ok(Some((
        FrameKind::TransferControl,
        AppEnvelope::new(
            message_type::TRANSFER_ACCEPT,
            manager.next_envelope_seq(),
            json!(TransferIdPayload { transfer_id }),
        ),
    )))
}

/// 发送方：发一块分片。返回 `Ok(false)` 表示这次没轮到（没有待发分片或 pacing 令牌不够）。
async fn send_next_chunk<S>(
    sink: &mut S,
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    pacer: &mut Pacer,
    chunk_pacer: &mut Pacer,
    direct_chunk_pacer: &mut Pacer,
    direct_leg: Option<&dyn link::ReliableLeg>,
) -> Result<bool, String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let Some(transfer_id) = state.next_sending_transfer() else {
        return Ok(false);
    };

    let route = state
        .transfers
        .get(&transfer_id)
        .map(|session| session.route)
        .unwrap_or_default();

    // 额度按**这一单钉住的那条腿**二选一（§7 / R23 / R32）。绝不两套都扣：DC 上的分片不
    // 经过中继的计费点，拿中继额度去压它会把 60Hz 的可覆盖流一起压死。
    let direct_leg = match route {
        link::Route::Relay => {
            // 两套额度都拿到才发（R18）：通用帧额度防止补发把中继扣穿，分片额度再留一层
            // 余量，否则 20/s 对 20/s 零余量，网络抖动一压缩到达间隔就会被 `close 1008` 打断
            if !Pacer::try_acquire_pair(pacer, chunk_pacer) {
                return Ok(false);
            }

            None
        }
        link::Route::Direct => {
            // 背压（R32）：翻假说明发送缓冲里已经积了几块，那条腿的 `send` 会**等**。
            // 这里是 `live` 的 `select!` 分支，等下去会把入站读取一起挡住，所以源头就不注入。
            let Some(leg) = direct_leg.filter(|leg| leg.writable()) else {
                return Ok(false);
            };

            if !direct_chunk_pacer.try_acquire() {
                return Ok(false);
            }

            Some(leg)
        }
    };

    let (frame, sent_bytes, complete) = {
        let session = state
            .transfers
            .get(&transfer_id)
            .ok_or_else(|| "传输会话不见了".to_string())?;
        let outgoing = session
            .outgoing
            .as_ref()
            .ok_or_else(|| "发送中的会话缺少发送状态".to_string())?;
        let index = outgoing.next_index();
        let chunk = outgoing.read_chunk(index)?;

        // 读出来的分片必须和 offer 里声明的分片大小一致，否则对方一定校验失败
        if chunk.len() != outgoing.expected_length() {
            return Err("读取到的附件分片大小不对".to_string());
        }

        let header = FrameHeader {
            kind: FrameKind::TransferChunk,
            flags: 0,
            transfer_id,
            seq: index,
        };
        let frame = state.transfer_cipher(transfer_id).seal(&header, &chunk)?;

        if frame.len() > MAX_BINARY_FRAME_SIZE {
            return Err("待发送的附件分片超过允许的大小".to_string());
        }

        (frame, chunk.len(), index + 1 >= outgoing.chunks)
    };

    match direct_leg {
        // `send` 是尽力而为、失败即丢（丢一帧 = 接收侧少一块 = 这一单报废），但它在源头
        // 已经被 `writable` 挡过：`High` 事件之后我们一块都不再注入，所以最多只会有一块
        // 在途。真正的收尾交给 `direct_lost()`（探针超时 / 通道关闭两处触发）。
        Some(leg) => leg.send(frame),
        None => send_frame(sink, Message::Binary(frame.into())).await?,
    }

    let Some(session) = state.transfers.get_mut(&transfer_id) else {
        return Ok(true);
    };

    if let Some(outgoing) = session.outgoing.as_mut() {
        outgoing.mark_sent(sent_bytes);
    }

    if complete {
        // 分片发完再告诉对方「发完了」，等它校验（§38）
        let envelope = AppEnvelope::new(
            message_type::TRANSFER_COMPLETE,
            manager.next_envelope_seq(),
            json!(TransferIdPayload { transfer_id }),
        );

        match state.queue(FrameKind::TransferControl, &envelope, false) {
            Ok(dropped) => {
                // 和别的入队点一样：队列满时旧帧会被挤掉，不能一声不响地吞下去
                if let Some(dropped) = dropped {
                    manager.emit_error(
                        manager.generation(),
                        "可靠发送队列已满，最旧的一条消息被丢弃".to_string(),
                    );
                    retry_dropped_chat(manager, state, &dropped);
                }

                // `transfer.complete` 必须走**同一单的**那条腿，而且要**绕过背压**：接收侧
                // 先看「分片收齐了没」，而两条腿之间没有顺序保证——最后一块还在 DC 的缓冲里、
                // 完成帧却从中继先到（背压在这几微秒里翻假就会这样），就会被判「缺分片」，
                // 一单白废。一帧的量交给那条腿排队即可，绝不绕路。
                flush(sink, state, pacer, direct_leg, direct_leg.is_some()).await?;
            }
            Err(error) => manager.emit_error(manager.generation(), error),
        }
    } else {
        report_progress(manager, session, "sending", None, false);
    }

    Ok(true)
}

/// 带超时的写入。对端不读数据时 `send` 会无限等待；超时后由调用方把连接判为断开。
///
/// 注意：所有**应用帧**都必须先过 `Pacer::acquire`（应用帧现在只有 `flush` 调用本函数），
/// 否则一次补发就会把中继的令牌桶扣穿、被 `close 1008` 踢掉。不走 pacer 的只有 WS
/// 控制帧：心跳 `Message::Ping`（本函数的唯一非应用帧调用点）与断开时的
/// `Message::Close`（直接 `sink.send`，不经过本函数），中继根本看不到它们。
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

/// 组一条**不走可靠队列**的应用帧（心跳与 P2P 信令）。
///
/// 不塞进 `state.reliable` 的理由：那条队列上限 512，队满时会挤掉最旧的聊天消息并把
/// 它退回 `pending`（`retry_dropped_chat`）。心跳与信令都是低流量，也不该扰动聊天。
fn build_frame(
    manager: &Arc<PairManager>,
    state: &mut SessionState,
    kind: FrameKind,
    message_type: &str,
    payload: Value,
) -> Result<Vec<u8>, String> {
    let envelope = AppEnvelope::new(message_type, manager.next_envelope_seq(), payload);

    state.encode(kind, &envelope)
}

type Reply = Option<(FrameKind, AppEnvelope)>;

fn handle_binary(
    manager: &Arc<PairManager>,
    generation: u64,
    state: &mut SessionState,
    bytes: &[u8],
    link: Option<&link::P2pLink>,
    lane: Option<link::Lane>,
) -> Result<Reply, String> {
    // 附件分片用 per-transfer 密钥（R17），所以先按明文帧头里的 kind / transferId 选密钥。
    // 帧头是 AEAD 的 associated data，选错密钥只会解密失败，不会绕过认证。
    let peeked = FrameHeader::decode(bytes).ok_or_else(|| "未知的帧类型".to_string())?;

    if peeked.kind == FrameKind::TransferChunk {
        let (header, plaintext) = state.transfer_cipher(peeked.transfer_id).open(bytes)?;

        if header.flags != 0 {
            return Err("收到不支持的帧标志".into());
        }

        return handle_transfer_chunk(manager, state, &header, &plaintext);
    }

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
        message_type::SIGNAL => {
            // R21：P2P 信令交给那条腿，不进 UI、也不产生回复。没有腿（非 Windows 目标）
            // 或载荷畸形就丢掉——对端本来就不可信。
            let Some(link) = link else {
                return Ok(None);
            };

            let Ok(signal) = serde_json::from_value::<PairSignalPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            link.handle_signal(signal);

            Ok(None)
        }
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

            manager.sink.emit(
                EVENT_PRESENCE,
                serde_json::to_value(payload).unwrap_or(Value::Null),
            );

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

            manager.sink.emit(
                EVENT_STATS,
                serde_json::to_value(stats).unwrap_or(Value::Null),
            );

            Ok(None)
        }
        message_type::CHAT_TEXT => {
            // 畸形或超长就直接丢掉：对端不可信，不能让一条坏消息打断网络层
            let Ok(payload) = serde_json::from_value::<ChatTextPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            if payload.text.trim().is_empty() || payload.text.len() > MESSAGE_TEXT_LIMIT {
                return Ok(None);
            }

            // 同一条消息重发（对端没收到 ack 时会重试）只补一次 ack，不重复入库与通知
            if manager.history.find(&payload.message_id)?.is_none() {
                let epoch = manager.history.epoch()?;
                let stored = manager.history.insert(&NewMessage::incoming_text(
                    payload.message_id.clone(),
                    payload.text,
                    now_millis(),
                    epoch,
                ))?;

                manager.sink.emit(
                    EVENT_MESSAGE_RECEIVED,
                    serde_json::to_value(stored).unwrap_or(Value::Null),
                );
            }

            Ok(Some((
                FrameKind::Ack,
                AppEnvelope::new(
                    message_type::CHAT_ACK,
                    manager.next_envelope_seq(),
                    json!({ "messageId": payload.message_id }),
                ),
            )))
        }
        message_type::CHAT_ACK => {
            let Ok(payload) = serde_json::from_value::<ChatAckPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            // ack 只对本机发出的消息有意义：对端如果 ack 一条 incoming 的 id，
            // 不该把本地那行「已收到」改成「对方已收到」
            if let Some(message) = manager.history.find(&payload.message_id)?
                && message.direction == MessageDirection::Outgoing
            {
                manager.publish_message_status(&payload.message_id, MessageStatus::Delivered);
            }

            Ok(None)
        }
        message_type::TRANSFER_OFFER => {
            // 载荷不合法就丢掉：对端本来就不可信
            let Ok(payload) = serde_json::from_value::<TransferOfferPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            handle_transfer_offer(manager, state, payload, lane)
        }
        message_type::TRANSFER_ACCEPT => {
            let Ok(payload) = serde_json::from_value::<TransferIdPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            let Some(session) = state.transfers.get_mut(&payload.transfer_id) else {
                return Ok(None);
            };

            // 只有发送方在等 accept
            if session.phase != TransferPhase::AwaitingAccept {
                return Ok(None);
            }

            session.phase = TransferPhase::Sending;

            report_progress(manager, session, "sending", None, true);

            // 空文件没有分片（§81 的 0 byte）：`transfer.complete` 平时由发最后一块的人捎带，
            // 这里没有「最后一块」，不补一条的话两边会停在 sending / receiving 直到断线
            if session_is_complete(session) {
                return Ok(Some((
                    FrameKind::TransferControl,
                    AppEnvelope::new(
                        message_type::TRANSFER_COMPLETE,
                        manager.next_envelope_seq(),
                        json!(TransferIdPayload { transfer_id: payload.transfer_id }),
                    ),
                )));
            }

            Ok(None)
        }
        message_type::TRANSFER_REJECT => {
            let Ok(payload) =
                serde_json::from_value::<TransferRejectPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            close_transfer(
                manager,
                state,
                payload.transfer_id,
                TransferOutcome::Failed,
                &format!("对方没有接收: {}", payload.reason),
            );

            Ok(None)
        }
        message_type::TRANSFER_COMPLETE => {
            let Ok(payload) = serde_json::from_value::<TransferIdPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            // 接收方：分片应该已经收齐了，没收齐说明中间丢了数据
            let missing = state
                .transfers
                .get(&payload.transfer_id)
                .and_then(|session| session.incoming.as_ref())
                .map(|incoming| incoming.received_chunks < incoming.chunks)
                .unwrap_or(false);

            let Some(session) = state.transfers.get(&payload.transfer_id) else {
                return Ok(None);
            };

            if session.phase != TransferPhase::Receiving {
                return Ok(None);
            }

            if missing {
                close_transfer(
                    manager,
                    state,
                    payload.transfer_id,
                    TransferOutcome::Failed,
                    "附件分片没有收全，请让对方重发",
                );

                return Ok(Some((
                    FrameKind::TransferControl,
                    AppEnvelope::new(
                        message_type::TRANSFER_VERIFIED,
                        manager.next_envelope_seq(),
                        json!(TransferVerifiedPayload {
                            transfer_id: payload.transfer_id,
                            ok: false,
                            message: Some("分片没有收全".to_string()),
                        }),
                    ),
                )));
            }

            finish_incoming_transfer(manager, state, payload.transfer_id)
        }
        message_type::TRANSFER_VERIFIED => {
            let Ok(payload) =
                serde_json::from_value::<TransferVerifiedPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            if payload.ok {
                if let Some(session) = state.transfers.get(&payload.transfer_id) {
                    manager.publish_message_status(&session.message_id, MessageStatus::Delivered);
                }

                close_transfer(manager, state, payload.transfer_id, TransferOutcome::Done, "");
            } else {
                let reason = payload
                    .message
                    .unwrap_or_else(|| "对方没有通过附件校验".to_string());

                close_transfer(
                    manager,
                    state,
                    payload.transfer_id,
                    TransferOutcome::Failed,
                    &reason,
                );
            }

            Ok(None)
        }
        message_type::TRANSFER_CANCEL => {
            let Ok(payload) = serde_json::from_value::<TransferIdPayload>(envelope.payload.clone())
            else {
                return Ok(None);
            };

            close_transfer(
                manager,
                state,
                payload.transfer_id,
                TransferOutcome::Canceled,
                "对方取消了这次传输",
            );

            Ok(None)
        }
        _ => {
            manager.sink.emit(
                EVENT_MESSAGE,
                serde_json::to_value(envelope).unwrap_or(Value::Null),
            );

            Ok(None)
        }
    }
}

fn handle_server_frame(
    manager: &Arc<PairManager>,
    generation: u64,
    text: &str,
) -> Result<Option<RelayConfig>, PairFailure> {
    // R18：text 只用于服务端控制帧，未知或畸形的控制帧按忽略处理。
    // 当成致命错误会让我们在中继新增一条控制帧时反复断线重连。
    let Ok(frame) = serde_json::from_str::<ServerFrame>(text) else {
        tauri_plugin_log::log::warn!("忽略无法解析的中继控制帧");

        return Ok(None);
    };

    match frame {
        ServerFrame::Welcome {
            protocol,
            peer_online,
            limits,
            ice_servers,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(PairFailure {
                    message: format!("中继协议版本不匹配: {protocol}"),
                    fatal: true,
                });
            }

            publish_peer(manager, generation, peer_online);

            // 中继没广告额度（旧中继）时就用 CF 的缺省推导，语义上「这次连接的有效配置」
            // 永远是确定的，会话层只管照着用
            Ok(Some(RelayConfig {
                limits: limits.unwrap_or_else(RelayLimits::cloudflare),
                // R21：没广告就是「没有 STUN/TURN，只有 host candidate」，这是隐私缺省
                ice_servers,
            }))
        }
        ServerFrame::Peer { online, device_id } => {
            if device_id == manager.device_id() {
                return Ok(None);
            }

            publish_peer(manager, generation, online);

            Ok(None)
        }
        ServerFrame::Error { code, message } => {
            manager.emit_error(generation, format!("中继错误 {code}: {message}"));

            Ok(None)
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

    if online {
        // §32：对方上线了，把离线期间攒下的消息补发出去
        manager.resend_pending_chat();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pair::protocol::{PetKeyboardState, PetPointerState};
    use crate::core::pair::transfer::{MIN_CHUNK_SIZE, sha256_file};

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
        let history = Arc::new(PairHistory::in_memory().unwrap());
        let manager = Arc::new(PairManager::new(
            "test-device".into(),
            sink.clone(),
            history,
            test_store(),
        ));

        (manager, sink)
    }

    /// 单测用的附件目录：每次一片新的临时目录，互不干扰
    fn test_store() -> TransferStore {
        TransferStore::new(
            std::env::temp_dir().join(format!("bongo-cat-pair-unit-{}", uuid::Uuid::new_v4())),
        )
    }

    /// 给 manager 装一条假的会话通道：这样 `send_chat` 会走「真的发出去」的分支
    fn with_session(manager: &Arc<PairManager>) -> mpsc::UnboundedReceiver<Command> {
        let (sender, receiver) = mpsc::unbounded_channel();

        *PairManager::lock(&manager.sender) = Some(sender);

        receiver
    }

    /// 伪造一条来自对端的加密帧，交给 `handle_binary` 处理
    fn deliver(
        manager: &Arc<PairManager>,
        state: &mut SessionState,
        kind: FrameKind,
        envelope: &AppEnvelope,
    ) -> Result<Reply, String> {
        let frame = PairCipher::new(&ROOT_KEY)
            .seal(&FrameHeader::new(kind, 0), &envelope.to_bytes().unwrap())
            .unwrap();

        handle_binary(manager, 0, state, &frame, None, None)
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

            assert!(dropped.is_none(), "第 {index} 条不应该触发丢弃");
        }

        let dropped = state
            .queue(
                FrameKind::Chat,
                &AppEnvelope::new(message_type::PING, u64::MAX, json!({})),
                false,
            )
            .unwrap();

        // 静默丢消息会让聊天永久缺一条；这里必须让调用方拿到被挤掉的那一条
        let dropped = dropped.expect("超出上限时必须报告丢弃");

        assert_eq!(dropped.message_type, message_type::PING);
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
            .send_replaceable(
                FrameKind::PetState,
                message_type::PET_STATE,
                snapshot.clone(),
            )
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

    /// 假的可覆盖腿：只记下收到的帧
    #[derive(Default)]
    struct FakeLeg {
        frames: Mutex<Vec<Vec<u8>>>,
    }

    impl FakeLeg {
        fn frames(&self) -> Vec<Vec<u8>> {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl link::CoverableLeg for FakeLeg {
        fn send(&self, frame: Vec<u8>) {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(frame);
        }
    }

    /// 只发二进制应用帧：第一字节就是明文的帧 kind（帧头是 AEAD 的 associated data）
    fn frame_kind(message: &Message) -> u8 {
        match message {
            Message::Binary(bytes) => bytes[0],
            other => panic!("应当只发二进制应用帧: {other:?}"),
        }
    }

    /// 假的可靠腿：记下收到的帧，并带一个可切换的背压标志（R32）
    struct FakeReliableLeg {
        frames: Mutex<Vec<Vec<u8>>>,
        writable: std::sync::atomic::AtomicBool,
    }

    impl FakeReliableLeg {
        fn new(writable: bool) -> Self {
            Self {
                frames: Mutex::new(Vec::new()),
                writable: std::sync::atomic::AtomicBool::new(writable),
            }
        }

        fn frames(&self) -> Vec<Vec<u8>> {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// 把背压翻假：发送缓冲积够了，源头该停手
        fn block(&self) {
            self.writable
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }

        /// 缓冲排空了，重新放行
        fn unblock(&self) {
            self.writable
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl link::ReliableLeg for FakeReliableLeg {
        fn send(&self, frame: Vec<u8>) {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(frame);
        }

        fn writable(&self) -> bool {
            self.writable.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// R30 / R32：可覆盖流在 `pet-state` 那条腿可用时**只**走它；聊天在 `reliable` 那条腿
    /// 可用时只走它、背压翻假就退回中继；腿不可用时全部回中继。
    ///
    /// 「这一帧没有经过服务器」这条负向断言只能在这里做：真中继看到的是密文，分不出帧
    /// kind，也没有任何计数器。
    #[tokio::test]
    async fn each_lane_keeps_its_own_stream_off_the_relay() {
        let mut state = SessionState::new(&ROOT_KEY);
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let mut direct_pacer = Pacer::new(DIRECT_FRAMES_PER_SECOND, DIRECT_BURST);
        let mut socket = RecordingSocket::default();
        let leg = FakeLeg::default();
        let reliable = FakeReliableLeg::new(true);

        state
            .queue(
                FrameKind::PetState,
                &AppEnvelope::new(message_type::PET_STATE, 0, json!({})),
                true,
            )
            .unwrap();
        state
            .queue(
                FrameKind::Chat,
                &AppEnvelope::new(message_type::CHAT_TEXT, 1, json!({ "text": "hi" })),
                false,
            )
            .unwrap();

        // 顺序与 `live` 一致：先可靠队列（聊天走可靠腿），后可覆盖队列（快照走可覆盖腿）
        flush(&mut socket, &mut state, &mut pacer, Some(&reliable), false)
            .await
            .unwrap();
        flush_replaceable(
            &mut socket,
            Some(&leg),
            &mut state,
            &mut pacer,
            &mut direct_pacer,
        )
        .await
        .unwrap();

        assert!(
            socket.sent.is_empty(),
            "两条腿都在时中继那条线上什么都不该有"
        );
        assert_eq!(reliable.frames().len(), 1);
        assert_eq!(reliable.frames()[0][0], FrameKind::Chat.as_byte());
        assert_eq!(leg.frames().len(), 1);
        assert_eq!(leg.frames()[0][0], FrameKind::PetState.as_byte());

        // R23：走 DC 的那两帧**都不吃中继的 pacer** —— 否则 DC 上的流会被压回 20 帧/秒、
        // 与 60Hz 冲突，而聊天在链路上根本不经过中继的计费点。反过来 DC 那条腿的额度
        // 被扣了一枚。
        assert!(
            (pacer.tokens - OUTBOUND_BURST).abs() < 0.05,
            "走 DC 的帧不该消耗中继的令牌: {}",
            pacer.tokens
        );
        assert!(
            direct_pacer.tokens <= DIRECT_BURST - 1.0 + 0.05,
            "DC 那条腿的额度应当被扣掉一枚: {}",
            direct_pacer.tokens
        );

        // R32 背压：可靠腿的发送缓冲积够了就退回中继，聊天一帧不丢
        state
            .queue(
                FrameKind::Chat,
                &AppEnvelope::new(message_type::CHAT_TEXT, 2, json!({ "text": "yo" })),
                false,
            )
            .unwrap();

        reliable.block();

        flush(&mut socket, &mut state, &mut pacer, Some(&reliable), false)
            .await
            .unwrap();

        assert_eq!(socket.sent.len(), 1, "背压时聊天必须退回中继");
        assert_eq!(frame_kind(&socket.sent[0]), FrameKind::Chat.as_byte());
        assert_eq!(reliable.frames().len(), 1, "背压时不该再往腿里塞");
        assert!(
            (pacer.tokens - (OUTBOUND_BURST - 1.0)).abs() < 0.05,
            "退回中继的帧必须照常吃中继的令牌: {}",
            pacer.tokens
        );

        // 腿不可用（DC 掉了、或者探针还没验过）：可覆盖流回到中继
        state
            .queue(
                FrameKind::Stats,
                &AppEnvelope::new(message_type::STATS, 2, json!({})),
                true,
            )
            .unwrap();

        flush_replaceable(&mut socket, None, &mut state, &mut pacer, &mut direct_pacer)
            .await
            .unwrap();

        assert_eq!(socket.sent.len(), 2);
        assert_eq!(frame_kind(&socket.sent[1]), FrameKind::Stats.as_byte());
        assert_eq!(leg.frames().len(), 1, "腿不该再收到任何东西");
    }

    /// R32：一单的**收尾帧**（`transfer.complete`）必须和分片走同一条 lane——背压翻假也
    /// 不能绕中继，否则「最后一块还在那条腿上、完成帧已从中继先到」会被判「缺分片」，
    /// 一单白废。普通帧仍然按背压回退中继。
    #[tokio::test]
    async fn a_closing_frame_stays_on_the_lane_it_was_pinned_to() {
        let mut state = SessionState::new(&ROOT_KEY);
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let mut socket = RecordingSocket::default();
        let leg = FakeReliableLeg::new(true);

        let complete = |seq: u64| {
            AppEnvelope::new(
                message_type::TRANSFER_COMPLETE,
                seq,
                json!(TransferIdPayload { transfer_id: 5 }),
            )
        };

        // 腿被背压挡住：普通路径整体回退中继
        leg.block();

        state
            .queue(FrameKind::TransferControl, &complete(1), false)
            .unwrap();

        flush(&mut socket, &mut state, &mut pacer, Some(&leg), false)
            .await
            .unwrap();

        assert_eq!(socket.sent.len(), 1);
        assert_eq!(
            frame_kind(&socket.sent[0]),
            FrameKind::TransferControl.as_byte()
        );
        assert!(leg.frames().is_empty(), "背压时普通帧不该再往腿里塞");

        // 同一帧、同一条腿，但这一批是「钉住的那一单的收尾」：必须走腿，绝不绕中继
        state
            .queue(FrameKind::TransferControl, &complete(2), false)
            .unwrap();

        flush(&mut socket, &mut state, &mut pacer, Some(&leg), true)
            .await
            .unwrap();

        assert_eq!(socket.sent.len(), 1, "收尾帧不该绕中继");
        assert_eq!(leg.frames().len(), 1);
        assert_eq!(leg.frames()[0][0], FrameKind::TransferControl.as_byte());
    }

    /// R30 的不变量：只有 `dc_open` 不够，必须同时 `dc_verified` 才把可覆盖流切过去
    #[tokio::test]
    async fn the_coverable_leg_needs_both_flags() {
        let (link, _events) = link::P2pLink::spawn("coverable-leg".into(), Vec::new());

        assert!(coverable_leg(true, false, &link).is_none());
        assert!(coverable_leg(false, true, &link).is_none());
        assert!(coverable_leg(true, true, &link).is_some());
    }

    /// R32 的不变量：可靠腿和可覆盖腿一样要**两个**标志（读的是另一条通道的那一对）
    #[tokio::test]
    async fn the_reliable_leg_needs_both_flags() {
        let (link, _events) = link::P2pLink::spawn("reliable-leg".into(), Vec::new());

        assert!(reliable_leg(true, false, &link).is_none());
        assert!(reliable_leg(false, true, &link).is_none());
        assert!(reliable_leg(true, true, &link).is_some());
    }

    /// R32：一单的**回执**按它钉住的 route 选腿，不看「此刻哪条腿可用」——钉在中继上的
    /// 那一单，它的 accept / reject / cancel 走 DC 一旦丢帧就没人收尾（`direct_lost()` 只管
    /// Direct 的会话），发送方会永远停在 `AwaitingAccept`。
    #[tokio::test]
    async fn replies_follow_the_route_the_transfer_was_pinned_to() {
        let (link, _events) = link::P2pLink::spawn("reply-leg".into(), Vec::new());

        // 中继那一单：可靠腿可用也不许走
        assert!(reply_leg(link::Route::Relay, true, true, &link).is_none());

        // Direct 那一单：两个标志都得满足（和选路同一把门）
        assert!(reply_leg(link::Route::Direct, true, true, &link).is_some());
        assert!(reply_leg(link::Route::Direct, true, false, &link).is_none());
        assert!(reply_leg(link::Route::Direct, false, true, &link).is_none());

        // 会话已经被收掉（未知 id）：按中继，回执宁愿绕路也不能丢
        let state = SessionState::new(&ROOT_KEY);

        assert_eq!(transfer_route(&state, 7), link::Route::Relay);
    }

    /// R32 / §8 的形状 3：offer 从哪条 lane 来，这一单就钉哪条（接收侧记下 lane），
    /// 回执也跟着它走同一条腿
    #[tokio::test]
    async fn an_offer_pins_the_transfer_to_the_lane_it_arrived_on() {
        let store = TransferStore::new(temp_root("offer-lane"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let (_source, size, sha256) = source_file(&root, &vec![3u8; 4096]);
        // 线上形状：走可靠腿的这一单就是 48 KiB（helper 只是通用，参数由调用方给）
        let offer = offer_with_chunk_size(
            41,
            TransferKind::File,
            "x.bin",
            size,
            &sha256,
            P2P_CHUNK_SIZE,
        );
        let frame = PairCipher::new(&ROOT_KEY)
            .seal(
                &FrameHeader::new(FrameKind::TransferControl, 0),
                &offer.to_bytes().unwrap(),
            )
            .unwrap();

        // 从可靠腿进来：小文件自动接收，回一条 accept
        let reply = handle_binary(
            &manager,
            0,
            &mut state,
            &frame,
            None,
            Some(link::Lane::Reliable),
        )
        .unwrap()
        .expect("小文件应当自动接收并回 accept");

        assert_eq!(reply.1.message_type, message_type::TRANSFER_ACCEPT);
        assert_eq!(
            transfer_route(&state, 41),
            link::Route::Direct,
            "offer 从可靠腿来，这一单就钉可靠腿"
        );

        // 这一单的回执因此也钉在可靠腿上（两个标志都满足时）
        let (link, _events) = link::P2pLink::spawn("offer-lane".into(), Vec::new());

        assert!(reply_leg(transfer_route(&state, 41), true, true, &link).is_some());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// R23 / R32：DC 上的分片额度与中继那两套桶完全独立，数值按「同样的字节速率」推导
    #[test]
    fn the_direct_chunk_budget_matches_the_relay_byte_rate() {
        // 15 × 512 KiB / 48 KiB = 160：DC 的分片小 512/48 倍，速率就按同一比例放大
        assert!(
            (DIRECT_CHUNKS_PER_SECOND
                - OUTBOUND_CHUNKS_PER_SECOND * CHUNK_SIZE as f64 / P2P_CHUNK_SIZE as f64)
                .abs()
                < 1e-9
        );
        assert_eq!(DIRECT_CHUNK_BURST, 16.0);
        // 三套桶的数值必须两两不同，否则某一天有人「顺手」把它们合并了也测不出来
        assert_ne!(DIRECT_CHUNKS_PER_SECOND, OUTBOUND_CHUNKS_PER_SECOND);
        assert_ne!(DIRECT_FRAMES_PER_SECOND, DIRECT_CHUNKS_PER_SECOND);
    }

    /// R18 / R32 的不变量：`chunk_wait_duration` 返回 `ZERO` 必须意味着紧接着一定发得出去
    #[test]
    fn the_chunk_wait_follows_the_route_that_is_actually_pinned() {
        let pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let chunk_pacer = Pacer::new(OUTBOUND_CHUNKS_PER_SECOND, OUTBOUND_CHUNK_BURST);
        let direct_chunk_pacer = Pacer::new(DIRECT_CHUNKS_PER_SECOND, DIRECT_CHUNK_BURST);
        let leg = FakeReliableLeg::new(true);

        // 没有待发分片：那一支直接 `pending()`，不看任何额度
        assert!(
            chunk_wait_duration(None, Some(&leg), &pacer, &chunk_pacer, &direct_chunk_pacer)
                .is_none()
        );

        // 桶是满的 → 立刻能发
        assert_eq!(
            chunk_wait_duration(
                Some(link::Route::Relay),
                None,
                &pacer,
                &chunk_pacer,
                &direct_chunk_pacer
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            chunk_wait_duration(
                Some(link::Route::Direct),
                Some(&leg),
                &pacer,
                &chunk_pacer,
                &direct_chunk_pacer
            ),
            Some(Duration::ZERO)
        );

        // Direct 那一单**不看**中继的两个桶（它一个令牌都不该被扣）：把中继桶抽干，
        // 等的时间仍然只由 DC 的分片额度决定
        let empty_relay = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, 0.0);

        let wait = chunk_wait_duration(
            Some(link::Route::Direct),
            Some(&leg),
            &empty_relay,
            &chunk_pacer,
            &direct_chunk_pacer,
        )
        .unwrap();

        assert_eq!(wait, Duration::ZERO, "Direct 不该等中继的额度");

        // 背压翻假：没有可计算的剩余时间，按固定间隔轮询（**不能**返回 ZERO，
        // 否则 `select!` 会在 `Ok(false)` 与 `ZERO` 之间空转）
        leg.block();

        assert_eq!(
            chunk_wait_duration(
                Some(link::Route::Direct),
                Some(&leg),
                &pacer,
                &chunk_pacer,
                &direct_chunk_pacer
            ),
            Some(DIRECT_RETRY_INTERVAL)
        );

        // 腿整个不见了（通道已关）：同样不能返回 ZERO
        assert_eq!(
            chunk_wait_duration(
                Some(link::Route::Direct),
                None,
                &pacer,
                &chunk_pacer,
                &direct_chunk_pacer
            ),
            Some(DIRECT_RETRY_INTERVAL)
        );

        // 中继那一单看的是两个桶里更晚的那一个
        let slow = Pacer::new(1.0, 0.0);

        assert_eq!(
            chunk_wait_duration(
                Some(link::Route::Relay),
                None,
                &slow,
                &chunk_pacer,
                &direct_chunk_pacer
            ),
            Some(slow.wait_duration().max(chunk_pacer.wait_duration()))
        );
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
    fn chat_text_is_stored_and_acked() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new(
            message_type::CHAT_TEXT,
            1,
            json!({ "messageId": "m1", "text": "你好" }),
        );

        let (kind, reply) = deliver(&manager, &mut state, FrameKind::Chat, &envelope)
            .unwrap()
            .expect("收到 chat.text 必须回 ack");

        assert_eq!(kind, FrameKind::Ack);
        assert_eq!(reply.message_type, message_type::CHAT_ACK);
        assert_eq!(reply.payload["messageId"], "m1");

        let stored = manager.history.find("m1").unwrap().unwrap();

        assert_eq!(stored.text.as_deref(), Some("你好"));
        assert_eq!(stored.direction, MessageDirection::Incoming);
        assert_eq!(stored.status, MessageStatus::Received);
        assert_eq!(sink.payloads(EVENT_MESSAGE_RECEIVED).len(), 1);
    }

    #[test]
    fn duplicate_chat_text_is_acked_but_stored_once() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);

        // 对端没收到 ack 时会重发：换一条信封（新 envelope id）再投一次
        for seq in [1, 2] {
            let envelope = AppEnvelope::new(
                message_type::CHAT_TEXT,
                seq,
                json!({ "messageId": "m1", "text": "你好" }),
            );

            assert!(
                deliver(&manager, &mut state, FrameKind::Chat, &envelope)
                    .unwrap()
                    .is_some()
            );
        }

        assert_eq!(manager.history.count(None).unwrap(), 1);
        assert_eq!(
            sink.payloads(EVENT_MESSAGE_RECEIVED).len(),
            1,
            "重发不该再通知一次"
        );
    }

    #[test]
    fn chat_ack_marks_the_message_delivered() {
        let (manager, sink) = test_manager();

        manager
            .history
            .insert(&NewMessage::outgoing_text("m1".into(), "你好".into(), 1, 1))
            .unwrap();

        let mut state = SessionState::new(&ROOT_KEY);
        let envelope = AppEnvelope::new(message_type::CHAT_ACK, 1, json!({ "messageId": "m1" }));

        assert!(
            deliver(&manager, &mut state, FrameKind::Ack, &envelope)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            manager.history.find("m1").unwrap().unwrap().status,
            MessageStatus::Delivered
        );
        assert_eq!(sink.payloads(EVENT_MESSAGE_UPDATED).len(), 1);
    }

    /// ack 只能改动本机发出的消息：对端 ack 一条自己发来的 id 不该动本地那行
    #[test]
    fn chat_ack_ignores_incoming_messages() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);

        manager
            .history
            .insert(&NewMessage::incoming_text("m1".into(), "你好".into(), 1, 1))
            .unwrap();

        let envelope = AppEnvelope::new(message_type::CHAT_ACK, 1, json!({ "messageId": "m1" }));

        assert!(
            deliver(&manager, &mut state, FrameKind::Ack, &envelope)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            manager.history.find("m1").unwrap().unwrap().status,
            MessageStatus::Received
        );
        assert!(sink.payloads(EVENT_MESSAGE_UPDATED).is_empty());
    }

    /// 可靠队列挤掉一条聊天帧时，那条消息要退回「等待发送」，不能停在「已发送」
    #[test]
    fn dropped_chat_frames_send_the_message_back_to_pending() {
        let (manager, sink) = test_manager();
        let stored = manager
            .history
            .insert(&NewMessage::outgoing_text("m1".into(), "你好".into(), 1, 1))
            .unwrap();

        manager.publish_message_status("m1", MessageStatus::Sent);

        assert_eq!(
            manager.history.find("m1").unwrap().unwrap().status,
            MessageStatus::Sent
        );

        retry_dropped_chat(
            &manager,
            &mut SessionState::new(&[3u8; 32]),
            &AppEnvelope::new(
                message_type::CHAT_TEXT,
                1,
                json!({ "messageId": stored.id, "text": "你好" }),
            ),
        );

        assert_eq!(
            manager.history.find("m1").unwrap().unwrap().status,
            MessageStatus::Pending
        );
        assert_eq!(sink.payloads(EVENT_MESSAGE_UPDATED).len(), 2);

        // 非聊天帧没有对应的消息，不能误改状态
        retry_dropped_chat(
            &manager,
            &mut SessionState::new(&[3u8; 32]),
            &AppEnvelope::new(message_type::CHAT_ACK, 2, json!({ "messageId": "m1" })),
        );
        assert_eq!(
            manager.history.find("m1").unwrap().unwrap().status,
            MessageStatus::Pending
        );
    }

    /// offer 被可靠性队列挤掉时：消息标 failed，**而且**那个等不到 accept 的会话要一起收掉，
    /// 否则它会一直占着 MAX_ACTIVE_TRANSFERS 的名额，用户只会看到「同时进行的传输太多」
    #[tokio::test]
    async fn a_dropped_offer_frees_the_transfer_slot() {
        let store = TransferStore::new(temp_root("dropped-offer"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let (source, size, sha256) = source_file(&root, &[1, 2, 3]);

        manager
            .history
            .insert(&NewMessage::outgoing_attachment(
                "m-1".into(),
                MessageKind::File,
                "a-1".into(),
                0,
                1,
            ))
            .unwrap();
        manager
            .history
            .upsert_attachment(&NewAttachment {
                id: "a-1".into(),
                kind: MessageKind::File,
                original_name: Some("x.bin".into()),
                mime: Some("application/octet-stream".into()),
                size: Some(size),
                sha256: Some(sha256.clone()),
                local_path: Some(source.to_string_lossy().to_string()),
                created_at: 0,
            })
            .unwrap();

        start_outgoing_transfer(
            &manager,
            &mut state,
            OutgoingRequest {
                transfer_id: 9,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "x.bin".into(),
                mime: "application/octet-stream".into(),
                size,
                sha256,
                path: source,
            },
            link::Route::Relay,
        )
        .unwrap();

        assert_eq!(state.transfers.len(), 1);

        retry_dropped_chat(
            &manager,
            &mut state,
            &AppEnvelope::new(
                message_type::TRANSFER_OFFER,
                1,
                json!({ "messageId": "m-1" }),
            ),
        );

        assert!(
            state.transfers.is_empty(),
            "被挤掉的 offer 不能继续占着传输名额"
        );
        assert!(manager.transfer_of("m-1").is_err());
        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Failed
        );
        assert!(
            sink.payloads(EVENT_TRANSFER)
                .iter()
                .any(|payload| payload["state"] == "failed")
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pacer_refills_at_the_configured_rate_and_caps_at_the_burst() {
        let rate = OUTBOUND_FRAMES_PER_SECOND;
        let burst = OUTBOUND_BURST;

        assert!(
            (Pacer::refill(0.0, 0.05, rate, burst) - 1.0).abs() < 1e-9,
            "50ms 应当补一枚令牌"
        );
        assert!(
            (Pacer::refill(5.0, 0.0, rate, burst) - 5.0).abs() < 1e-9,
            "没有经过时间就不补充"
        );
        assert!(
            (Pacer::refill(19.5, 30.0, rate, burst) - burst).abs() < 1e-9,
            "长时间空闲也不能攒成无限突发"
        );
        assert!(
            (Pacer::refill(1.0, -5.0, rate, burst) - 1.0).abs() < 1e-9,
            "时钟异常不该扣令牌"
        );
    }

    /// 分片要同时过两套额度，而两套必须**一起扣**：只够一边时一枚都不能扣，
    /// 否则会出现「白扣一枚通用令牌却什么都没发」。
    #[test]
    fn acquiring_two_pacers_never_spends_only_one() {
        // 补充量按经过时间算，这里让两次调用之间只隔几百微秒，容差取 0.05 枚
        const EPS: f64 = 0.05;

        let mut frame = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let mut chunk = Pacer::new(OUTBOUND_CHUNKS_PER_SECOND, OUTBOUND_CHUNK_BURST);

        // 分片额度不够：通用额度也不能被扣
        frame.tokens = 10.0;
        chunk.tokens = 0.5;
        frame.updated_at = tokio::time::Instant::now();
        chunk.updated_at = tokio::time::Instant::now();

        assert!(!Pacer::try_acquire_pair(&mut frame, &mut chunk));
        assert!((frame.tokens - 10.0).abs() < EPS, "通用额度被白扣了");
        assert!((chunk.tokens - 0.5).abs() < EPS, "分片额度不该被扣");

        // 反过来同理
        frame.tokens = 0.5;
        chunk.tokens = 10.0;
        frame.updated_at = tokio::time::Instant::now();
        chunk.updated_at = tokio::time::Instant::now();

        assert!(!Pacer::try_acquire_pair(&mut frame, &mut chunk));
        assert!((frame.tokens - 0.5).abs() < EPS);
        assert!((chunk.tokens - 10.0).abs() < EPS, "分片额度被白扣了");

        // 两边都够才各扣一枚
        frame.tokens = 3.0;
        chunk.tokens = 3.0;
        frame.updated_at = tokio::time::Instant::now();
        chunk.updated_at = tokio::time::Instant::now();

        assert!(Pacer::try_acquire_pair(&mut frame, &mut chunk));
        assert!((frame.tokens - 2.0).abs() < EPS);
        assert!((chunk.tokens - 2.0).abs() < EPS);
    }

    /// 客户端自己也得守住中继的预算，否则一次补发就会被 close 1008
    #[test]
    fn outbound_pacing_stays_within_the_relay_budget() {
        // server-cloudflare/src/protocol.ts: MAX_FRAMES_PER_SECOND = 30 / MAX_CHUNKS_PER_SECOND = 20
        const RELAY_FRAMES_PER_SECOND: f64 = 30.0;
        const RELAY_CHUNKS_PER_SECOND: f64 = 20.0;

        assert!(OUTBOUND_BURST < RELAY_FRAMES_PER_SECOND);
        assert!(OUTBOUND_FRAMES_PER_SECOND < RELAY_FRAMES_PER_SECOND);

        // 分片这一路要和中继的分片桶留出余量：贴死之后任何到达间隔抖动都会扣穿它
        assert!(OUTBOUND_CHUNK_BURST < RELAY_CHUNKS_PER_SECOND);
        assert!(OUTBOUND_CHUNKS_PER_SECOND < RELAY_CHUNKS_PER_SECOND);
    }

    /// R20：缺省的四元组必须就是 CF 广告值的推导结果，否则「不改行为」的承诺会漂
    #[test]
    fn the_default_pacing_is_the_cloudflare_derivation() {
        let derived = RelayLimits::cloudflare().outbound();

        assert_eq!(derived.frames_per_second, OUTBOUND_FRAMES_PER_SECOND);
        assert_eq!(derived.frames_burst, OUTBOUND_BURST);
        assert_eq!(derived.chunks_per_second, OUTBOUND_CHUNKS_PER_SECOND);
        assert_eq!(derived.chunks_burst, OUTBOUND_CHUNK_BURST);
    }

    /// R20：中继广告更高额度时客户端跟着放大，但仍然留余量
    #[test]
    fn retuning_the_pacer_follows_the_advertised_limits() {
        let mut frame = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let outbound = RelayLimits {
            frames_per_second: 90.0,
            ..RelayLimits::cloudflare()
        }
        .outbound();

        frame.retune(outbound.frames_per_second, outbound.frames_burst);

        assert_eq!(frame.rate, 60.0);
        assert_eq!(frame.burst, 60.0);
        assert!(frame.tokens <= frame.burst);
    }

    /// R23：DC 那条腿的额度就是 §6 的上限本身，与中继广告的额度**无关**，也不参与
    /// `retune`——那条腿上的帧压根不经过中继的计费点。
    #[test]
    fn the_direct_pacer_carries_the_sixty_hertz_budget() {
        let direct = Pacer::new(DIRECT_FRAMES_PER_SECOND, DIRECT_BURST);

        assert_eq!(direct.rate, MAX_PET_STATE_HZ);
        assert_eq!(direct.burst, MAX_PET_STATE_HZ);
        assert!(direct.rate > OUTBOUND_FRAMES_PER_SECOND);
    }

    /// §6 / R23：快照上限只跟**当前生效传输**的额度走。CF 缺省（30 → 推导 20）必须留在
    /// 3Hz —— 这一条就是「CF 版行为一字不变」；自建中继广告 90（推导 60）与 P2P 才是 60。
    #[test]
    fn the_pet_state_ceiling_follows_the_effective_transport() {
        let cloudflare = RelayLimits::cloudflare().outbound();
        let self_hosted = RelayLimits {
            frames_per_second: 90.0,
            ..RelayLimits::cloudflare()
        }
        .outbound();

        assert_eq!(
            pet_state_hz(false, cloudflare.frames_per_second),
            DEFAULT_PET_STATE_HZ
        );
        assert_eq!(
            pet_state_hz(false, self_hosted.frames_per_second),
            MAX_PET_STATE_HZ
        );
        assert_eq!(
            pet_state_hz(true, cloudflare.frames_per_second),
            MAX_PET_STATE_HZ
        );

        // 离谱的广告值既不能推成 0，也不能超过上限
        assert_eq!(pet_state_hz(false, 0.0), DEFAULT_PET_STATE_HZ);
        assert_eq!(pet_state_hz(false, 240.0), MAX_PET_STATE_HZ);
        assert_eq!(pet_state_hz(false, f64::NAN), DEFAULT_PET_STATE_HZ);
    }

    /// 缩容时不能留着超过容量的令牌，否则一次补发就能把中继的桶扣穿
    #[test]
    fn retuning_downwards_clamps_the_buffered_tokens() {
        let mut frame = Pacer::new(20.0, 20.0);

        frame.tokens = 20.0;
        frame.retune(2.0, 2.0);

        assert_eq!(frame.rate, 2.0);
        assert_eq!(frame.burst, 2.0);
        assert!(frame.tokens <= 2.0, "缩容后不该留下超过容量的令牌");
    }

    /// 离谱的额度（0 / NaN）不该把 Pacer 弄坏：按原样保留
    #[test]
    fn retuning_ignores_insane_values() {
        let mut frame = Pacer::new(20.0, 20.0);

        frame.retune(0.0, 0.0);
        assert_eq!(frame.rate, 20.0);
        assert_eq!(frame.burst, 20.0);

        frame.retune(f64::NAN, f64::NAN);
        assert_eq!(frame.rate, 20.0);
        assert_eq!(frame.burst, 20.0);
    }

    /// 单看「突发 < 中继上限」还不够：真正的不变量是**任意时刻累计放行量**都不超过中继的
    /// 令牌桶，否则第一次滚动秒里 20 突发 + 20 补充就会追上中继的 30 容量。
    #[test]
    fn cumulative_pacing_never_drains_the_relay_bucket() {
        // server-cloudflare/src/protocol.ts: MAX_FRAMES_PER_SECOND = 30，容量同为 30
        const RELAY_FRAMES_PER_SECOND: f64 = 30.0;
        const RELAY_BURST: f64 = 30.0;
        const STEP_SECS: f64 = 0.005;
        const STEPS: usize = 20_000;

        let mut client_tokens = OUTBOUND_BURST;
        let mut relay_tokens = RELAY_BURST;
        let mut released = 0usize;

        for step in 1..=STEPS {
            client_tokens = Pacer::refill(
                client_tokens,
                STEP_SECS,
                OUTBOUND_FRAMES_PER_SECOND,
                OUTBOUND_BURST,
            );
            relay_tokens = (relay_tokens + STEP_SECS * RELAY_FRAMES_PER_SECOND).min(RELAY_BURST);

            if client_tokens < 1.0 {
                continue;
            }

            client_tokens -= 1.0;
            relay_tokens -= 1.0;
            released += 1;

            let elapsed = step as f64 * STEP_SECS;

            assert!(
                relay_tokens >= 0.0,
                "中继令牌桶被扣穿：第 {released} 帧，t = {elapsed}s"
            );
            assert!(
                released as f64 <= RELAY_BURST + RELAY_FRAMES_PER_SECOND * elapsed + 1e-9,
                "累计放行量超过中继预算：第 {released} 帧，t = {elapsed}s"
            );
        }

        // 第一秒正好放行 20 突发 + 20 补充，仍在中继的 60 以内
        assert!(released > 1_000, "模拟没有真的发送：{released} 帧");
    }

    /// 分片这一路还要再模拟一次中继的**分片桶**（20 个/秒、容量 20）：
    /// 客户端只要贴到 20/s，中继的桶就会长期悬在 0，抖动一下即 `close 1008`，传输中途断掉。
    #[test]
    fn chunk_pacing_keeps_the_relay_chunk_bucket_in_the_black() {
        const RELAY_CHUNKS_PER_SECOND: f64 = 20.0;
        const RELAY_BURST: f64 = 20.0;
        const STEP_SECS: f64 = 0.005;
        const STEPS: usize = 20_000;

        let mut client_tokens = OUTBOUND_CHUNK_BURST;
        let mut relay_tokens = RELAY_BURST;
        let mut released = 0usize;
        let mut worst = f64::MAX;

        for step in 1..=STEPS {
            client_tokens = Pacer::refill(
                client_tokens,
                STEP_SECS,
                OUTBOUND_CHUNKS_PER_SECOND,
                OUTBOUND_CHUNK_BURST,
            );
            relay_tokens = (relay_tokens + STEP_SECS * RELAY_CHUNKS_PER_SECOND).min(RELAY_BURST);

            if client_tokens < 1.0 {
                continue;
            }

            client_tokens -= 1.0;
            relay_tokens -= 1.0;
            released += 1;
            worst = worst.min(relay_tokens);

            assert!(
                relay_tokens >= 0.0,
                "中继分片桶被扣穿：第 {released} 块，t = {}s",
                step as f64 * STEP_SECS
            );
        }

        assert!(released > 1_000, "模拟没有真的发送：{released} 块");
        // 留出的余量要看得见：全程中继桶都不该被压到 5 枚以下
        assert!(worst >= 5.0, "分片限速余量太小：最低只剩 {worst} 枚");
    }

    #[test]
    fn malformed_or_oversized_chat_payloads_are_ignored() {
        let (manager, sink) = test_manager();
        let mut state = SessionState::new(&ROOT_KEY);
        let oversized = "x".repeat(MESSAGE_TEXT_LIMIT + 1);

        for payload in [
            json!({ "messageId": "m1" }),
            json!({ "messageId": "m1", "text": "   " }),
            json!({ "messageId": "m2", "text": oversized }),
        ] {
            let envelope = AppEnvelope::new(message_type::CHAT_TEXT, 1, payload);

            assert!(
                deliver(&manager, &mut state, FrameKind::Chat, &envelope)
                    .unwrap()
                    .is_none()
            );
        }

        assert_eq!(manager.history.count(None).unwrap(), 0);
        assert!(sink.payloads(EVENT_MESSAGE_RECEIVED).is_empty());
    }

    #[test]
    fn send_chat_rejects_empty_and_oversized_text() {
        let (manager, _sink) = test_manager();

        assert!(manager.send_chat("").is_err());
        assert!(manager.send_chat("   ").is_err());
        assert!(
            manager
                .send_chat(&"x".repeat(MESSAGE_TEXT_LIMIT + 1))
                .is_err()
        );
        assert_eq!(
            manager.history.count(None).unwrap(),
            0,
            "被拒绝的消息不该入库"
        );
    }

    #[test]
    fn sending_chat_marks_it_sent_and_queues_one_frame() {
        let (manager, sink) = test_manager();
        let mut receiver = with_session(&manager);

        let message = manager.send_chat("你好").unwrap();

        assert_eq!(message.status, MessageStatus::Sent);

        match receiver.try_recv().expect("应当排一条 chat 帧") {
            Command::Send { kind, envelope } => {
                assert_eq!(kind, FrameKind::Chat);
                assert_eq!(envelope.message_type, message_type::CHAT_TEXT);
                assert_eq!(envelope.payload["messageId"], message.id);
                assert_eq!(envelope.payload["text"], "你好");
            }
            _ => panic!("排队的应当是一条 chat 帧"),
        }

        assert_eq!(sink.payloads(EVENT_MESSAGE_UPDATED).len(), 1);
    }

    #[test]
    fn offline_chat_stays_pending_until_the_peer_comes_back() {
        let (manager, _sink) = test_manager();

        // 没有会话：消息留在本地 pending（§32）
        let message = manager.send_chat("回来叫我").unwrap();

        assert_eq!(message.status, MessageStatus::Pending);
        assert_eq!(manager.history.pending(10).unwrap().len(), 1);

        // 对端上线：一定要补发
        let mut receiver = with_session(&manager);

        manager.resend_pending_chat();

        assert!(matches!(
            receiver.try_recv().expect("上线后应当补发"),
            Command::Send { .. }
        ));
        assert_eq!(
            manager.history.find(&message.id).unwrap().unwrap().status,
            MessageStatus::Sent
        );
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

    /// 记录所有写出去的帧，用来验证分片的实际线格式
    #[derive(Default)]
    struct RecordingSocket {
        sent: Vec<Message>,
    }

    impl futures_util::Sink<Message> for RecordingSocket {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(
            mut self: std::pin::Pin<&mut Self>,
            item: Message,
        ) -> Result<(), Self::Error> {
            self.sent.push(item);

            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// 一个带真实附件目录的 manager（附件要真的落盘）
    fn manager_with_store(
        store: TransferStore,
    ) -> (Arc<PairManager>, Arc<TestSink>, std::path::PathBuf) {
        let root = store.root().to_path_buf();

        store.ensure().unwrap();

        let sink = Arc::new(TestSink::default());
        let manager = Arc::new(PairManager::new(
            "test-device".into(),
            sink.clone(),
            Arc::new(PairHistory::in_memory().unwrap()),
            store,
        ));

        (manager, sink, root)
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bongo-cat-pair-{tag}-{}",
            uuid::Uuid::new_v4()
        ));

        std::fs::create_dir_all(&root).unwrap();

        root
    }

    /// 写一个真实文件当发送源，返回 (路径, 大小, sha256)
    fn source_file(root: &std::path::Path, bytes: &[u8]) -> (std::path::PathBuf, u64, String) {
        let path = root.join("source.bin");

        std::fs::write(&path, bytes).unwrap();

        let (sha256, size) = sha256_file(&path).unwrap();

        (path, size, sha256)
    }

    fn offer_for(
        transfer_id: u64,
        kind: TransferKind,
        name: &str,
        size: u64,
        sha256: &str,
    ) -> AppEnvelope {
        offer_with_chunk_size(transfer_id, kind, name, size, sha256, CHUNK_SIZE)
    }

    /// 指定分片大小的 offer（§7 / R32）：中继 512 KiB、P2P 48 KiB 都要能走
    fn offer_with_chunk_size(
        transfer_id: u64,
        kind: TransferKind,
        name: &str,
        size: u64,
        sha256: &str,
        chunk_size: usize,
    ) -> AppEnvelope {
        AppEnvelope::new(
            message_type::TRANSFER_OFFER,
            1,
            serde_json::to_value(TransferOfferPayload {
                transfer_id,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind,
                name: name.into(),
                size,
                mime: "application/octet-stream".into(),
                sha256: sha256.into(),
                chunk_size: chunk_size as u32,
                chunks: chunk_count(size, chunk_size),
            })
            .unwrap(),
        )
    }

    /// 用 per-transfer 密钥发一块分片
    fn deliver_chunk(
        manager: &Arc<PairManager>,
        state: &mut SessionState,
        transfer_id: u64,
        index: u32,
        bytes: &[u8],
    ) -> Result<Reply, String> {
        let header = FrameHeader {
            kind: FrameKind::TransferChunk,
            flags: 0,
            transfer_id,
            seq: index,
        };
        let frame = PairCipher::new(&crypto::derive_transfer_key(&ROOT_KEY, transfer_id))
            .seal(&header, bytes)
            .unwrap();

        handle_binary(manager, 0, state, &frame, None, None)
    }

    fn count_part_files(root: &std::path::Path) -> usize {
        std::fs::read_dir(root.join("tmp"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry.path().extension().and_then(|value| value.to_str()) == Some("part")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// §81 的主路径：offer → accept → 分片 → 校验通过 → 落进附件目录
    #[test]
    fn a_received_file_lands_in_the_cache_after_verification() {
        let root = temp_root("receive");
        let store = TransferStore::new(&root);
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        // 跨块边界：一块半多一点
        let payload: Vec<u8> = (0..(CHUNK_SIZE + 7)).map(|index| (index % 251) as u8).collect();
        let (source, size, sha256) = source_file(&root, &payload);

        std::fs::remove_file(&source).unwrap();

        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(77, TransferKind::File, r"C:\Users\cat\秘密.zip", size, &sha256),
        )
        .unwrap();

        assert!(reply.is_some(), "小文件应当自动接收并回 accept");
        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Pending
        );

        for index in 0..chunk_count(size, CHUNK_SIZE) {
            let start = index as usize * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE).min(payload.len());
            let reply =
                deliver_chunk(&manager, &mut state, 77, index, &payload[start..end]).unwrap();

            if index + 1 < chunk_count(size, CHUNK_SIZE) {
                assert!(reply.is_none(), "中间的分片不该有回执");
            } else {
                let (_, verified) = reply.expect("最后一块应当回 verified");

                assert_eq!(verified.message_type, message_type::TRANSFER_VERIFIED);
                assert_eq!(verified.payload["ok"], true);
            }
        }

        let message = manager.history.find("m-1").unwrap().unwrap();

        assert_eq!(message.status, MessageStatus::Received);
        assert_eq!(message.kind, MessageKind::File);

        let attachment = message.attachment.expect("消息要带上附件记录");
        let path = attachment.local_path.expect("收完要有落盘路径");

        assert_eq!(attachment.original_name.as_deref(), Some("秘密.zip"));
        assert_eq!(attachment.size, Some(size));
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        // 落盘用的是 UUID + 扩展名，绝不是对方的原始文件名（§42）
        let file_name = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert!(file_name.ends_with(".zip"), "{file_name}");
        assert!(!file_name.contains("秘密"), "{file_name}");
        assert_eq!(count_part_files(&root), 0, "不该留下 .part");

        let events = sink.payloads(EVENT_TRANSFER);

        assert!(events.iter().any(|payload| payload["state"] == "done"));
        assert!(events.iter().any(|payload| payload["percent"] == 100));

        assert!(state.transfers.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §81：hash 不对要拒绝、删掉半成品、标记失败
    #[test]
    fn a_hash_mismatch_fails_the_message_and_deletes_the_copy() {
        let store = TransferStore::new(temp_root("hash-bad"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload = vec![3u8; 1024];
        let (_, size, _) = source_file(&root, &payload);

        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(9, TransferKind::File, "a.bin", size, &"0".repeat(64)),
        )
        .unwrap();

        let reply = deliver_chunk(&manager, &mut state, 9, 0, &payload).unwrap();
        let (_, verified) = reply.expect("校验失败也要回 verified");

        assert_eq!(verified.payload["ok"], false);

        let message = manager.history.find("m-1").unwrap().unwrap();

        assert_eq!(message.status, MessageStatus::Failed);
        assert!(message.attachment.unwrap().local_path.is_none());
        assert_eq!(count_part_files(&root), 0);
        assert!(state.transfers.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §42 / §81：超过上限的 offer 直接拒绝，磁盘与数据库都不动
    #[test]
    fn an_oversized_offer_is_rejected_without_touching_the_disk() {
        let store = TransferStore::new(temp_root("oversize"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        assert_eq!(manager.set_max_attachment_mb(1), 1024 * 1024);

        let size = 2 * 1024 * 1024;
        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(3, TransferKind::File, "big.bin", size, &"a".repeat(64)),
        )
        .unwrap();
        let (_, reject) = reply.expect("超限的 offer 应当被拒绝");

        assert_eq!(reject.message_type, message_type::TRANSFER_REJECT);
        assert!(manager.history.find("m-1").unwrap().is_none());
        assert!(manager.history.attachment("a-1").unwrap().is_none());
        assert!(state.transfers.is_empty());
        assert!(sink.payloads(EVENT_TRANSFER).is_empty());
        assert_eq!(count_part_files(&root), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §42：普通大文件先问用户，确认之前一块都不收
    #[test]
    fn a_large_file_waits_for_the_user_before_receiving_chunks() {
        let store = TransferStore::new(temp_root("confirm"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let size = 50 * 1024 * 1024 + 1;
        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(11, TransferKind::File, "movie.mkv", size, &"b".repeat(64)),
        )
        .unwrap();

        assert!(reply.is_none(), "等用户确认之前不该回 accept");

        let session = state.transfers.get(&11).expect("会话应当在等确认");

        assert_eq!(session.phase, TransferPhase::AwaitingDecision);
        assert!(session.incoming.is_none(), "还没确认就不该建临时文件");
        assert_eq!(count_part_files(&root), 0);
        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Pending
        );

        let events = sink.payloads(EVENT_TRANSFER);

        assert!(events.iter().any(|payload| payload["state"] == "waiting"));

        // 用户点「接收」之后才开始建临时文件并回 accept
        let reply = accept_incoming_transfer(&manager, &mut state, 11).unwrap();

        assert!(reply.is_some());
        assert_eq!(state.transfers.get(&11).unwrap().phase, TransferPhase::Receiving);
        assert_eq!(count_part_files(&root), 1);

        // 收尾：删掉临时文件，别把测试垃圾留在临时目录里
        abort_transfers(&manager, &mut state);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §44 / §45：语音走同一套 transfer，但和图片一样不用用户点「接收」
    #[test]
    fn a_voice_offer_starts_receiving_right_away() {
        let store = TransferStore::new(temp_root("voice"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload = vec![7u8; CHUNK_SIZE + 1];
        let (_, size, sha256) = source_file(&root, &payload);

        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(
                31,
                TransferKind::Voice,
                "voice-20260923-150000.wav",
                size,
                &sha256,
            ),
        )
        .unwrap();

        assert!(reply.is_some(), "语音不该等用户确认");
        assert_eq!(
            state.transfers.get(&31).unwrap().phase,
            TransferPhase::Receiving
        );
        assert_eq!(count_part_files(&root), 1, "应当立刻开始收分片");

        let message = manager.history.find("m-1").unwrap().unwrap();

        assert_eq!(message.kind, MessageKind::Voice);
        assert!(
            !sink
                .payloads(EVENT_TRANSFER)
                .iter()
                .any(|payload| payload["state"] == "waiting"),
            "语音不该出现“等用户确认”这一步"
        );

        abort_transfers(&manager, &mut state);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 同一个 transferId 的 offer 重发不该建第二个会话
    #[test]
    fn a_duplicate_offer_is_ignored() {
        let store = TransferStore::new(temp_root("duplicate"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload = vec![1u8; 64];
        let (_, size, sha256) = source_file(&root, &payload);
        let offer = offer_for(5, TransferKind::Image, "cat.png", size, &sha256);

        assert!(
            deliver(&manager, &mut state, FrameKind::TransferControl, &offer)
                .unwrap()
                .is_some()
        );

        let again = deliver(&manager, &mut state, FrameKind::TransferControl, &offer).unwrap();

        assert!(again.is_none(), "重复 offer 不该再回一次 accept");
        assert_eq!(state.transfers.len(), 1);

        abort_transfers(&manager, &mut state);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 发送侧完整走一遍：offer → accept → 分片 → complete → verified
    #[tokio::test]
    async fn the_sender_walks_offer_accept_chunks_complete_and_verified() {
        let store = TransferStore::new(temp_root("send"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload: Vec<u8> = (0..(CHUNK_SIZE * 2 + 5))
            .map(|index| (index % 97) as u8)
            .collect();
        let (source, size, sha256) = source_file(&root, &payload);

        let mut socket = RecordingSocket::default();
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let mut chunk_pacer = Pacer::new(OUTBOUND_CHUNKS_PER_SECOND, OUTBOUND_CHUNK_BURST);
        let mut direct_chunk_pacer = Pacer::new(DIRECT_CHUNKS_PER_SECOND, DIRECT_CHUNK_BURST);

        manager
            .history
            .insert(&NewMessage::outgoing_attachment(
                "m-1".into(),
                MessageKind::File,
                "a-1".into(),
                0,
                1,
            ))
            .unwrap();
        manager
            .history
            .upsert_attachment(&NewAttachment {
                id: "a-1".into(),
                kind: MessageKind::File,
                original_name: Some("source.bin".into()),
                mime: Some("application/octet-stream".into()),
                size: Some(size),
                sha256: Some(sha256.clone()),
                local_path: Some(source.to_string_lossy().to_string()),
                created_at: 0,
            })
            .unwrap();

        start_outgoing_transfer(
            &manager,
            &mut state,
            OutgoingRequest {
                transfer_id: 5,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "source.bin".into(),
                mime: "application/octet-stream".into(),
                size,
                sha256,
                path: source,
            },
            link::Route::Relay,
        )
        .unwrap();

        assert_eq!(state.transfers.get(&5).unwrap().phase, TransferPhase::AwaitingAccept);
        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Sent
        );

        // 真实流程里连接任务在处理 StartTransfer 之后立刻 flush：offer 先出去
        flush(&mut socket, &mut state, &mut pacer, None, false)
            .await
            .unwrap();

        // 对端同意
        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_ACCEPT,
                2,
                json!(TransferIdPayload { transfer_id: 5 }),
            ),
        )
        .unwrap();

        assert_eq!(state.transfers.get(&5).unwrap().phase, TransferPhase::Sending);

        while send_next_chunk(
            &mut socket,
            &manager,
            &mut state,
            &mut pacer,
            &mut chunk_pacer,
            &mut direct_chunk_pacer,
            None,
        )
        .await
        .unwrap()
        {}

        // 线格式：第一帧是 offer（根密钥），随后是分片（per-transfer 密钥），最后一帧是 complete
        assert_eq!(
            socket.sent.len(),
            1 + chunk_count(size, CHUNK_SIZE) as usize + 1
        );

        let Message::Binary(offer_frame) = &socket.sent[0] else {
            panic!("第一个应当是二进制帧");
        };
        let (offer_header, offer_plain) = PairCipher::new(&ROOT_KEY).open(offer_frame).unwrap();

        assert_eq!(offer_header.kind, FrameKind::TransferControl);

        let envelope = AppEnvelope::from_bytes(&offer_plain).unwrap();

        assert_eq!(envelope.message_type, message_type::TRANSFER_OFFER);

        let offer: TransferOfferPayload = serde_json::from_value(envelope.payload).unwrap();

        assert_eq!(offer.transfer_id, 5);
        assert_eq!(offer.chunks, chunk_count(size, CHUNK_SIZE));
        assert_eq!(
            offer.sha256,
            manager
                .history
                .attachment("a-1")
                .unwrap()
                .unwrap()
                .sha256
                .unwrap()
        );

        // 分片拼回来必须和源文件逐字节一致
        let cipher = PairCipher::new(&crypto::derive_transfer_key(&ROOT_KEY, 5));
        let mut rebuilt = Vec::new();

        for (index, message) in socket.sent[1..socket.sent.len() - 1].iter().enumerate() {
            let Message::Binary(frame) = message else {
                panic!("分片必须是二进制帧");
            };
            let (header, plain) = cipher.open(frame).unwrap();

            assert_eq!(header.kind, FrameKind::TransferChunk);
            assert_eq!(header.transfer_id, 5);
            assert_eq!(header.seq, index as u32);

            rebuilt.extend_from_slice(&plain);
        }

        assert_eq!(rebuilt, payload);

        let Message::Binary(complete_frame) = socket.sent.last().unwrap() else {
            panic!("最后一帧必须是二进制帧");
        };
        let (_, complete_plain) = PairCipher::new(&ROOT_KEY).open(complete_frame).unwrap();
        let complete = AppEnvelope::from_bytes(&complete_plain).unwrap();

        assert_eq!(complete.message_type, message_type::TRANSFER_COMPLETE);

        // 对方校验通过
        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_VERIFIED,
                3,
                json!(TransferVerifiedPayload {
                    transfer_id: 5,
                    ok: true,
                    message: None,
                }),
            ),
        )
        .unwrap();

        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Delivered
        );
        assert!(state.transfers.is_empty());
        assert!(
            manager.transfer_of("m-1").is_err(),
            "结束后不该再留下可操作的传输"
        );
        assert!(sink.payloads(EVENT_TRANSFER).iter().any(|payload| payload["state"] == "done"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §7 / R32：钉在 DC 上的那一单用 48 KiB 分片、整单（offer / 分片 / complete）都走腿，
    /// 中继上一条帧都没有；额度也是自己的那一套，两边都不碰对方的桶。
    #[tokio::test]
    async fn a_direct_transfer_takes_the_leg_and_its_own_budget() {
        let store = TransferStore::new(temp_root("send-direct"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        // 两块多一点：3 块，块边界都要走到
        let payload: Vec<u8> = (0..(P2P_CHUNK_SIZE * 2 + 1))
            .map(|index| (index % 89) as u8)
            .collect();
        let (source, size, sha256) = source_file(&root, &payload);

        let mut socket = RecordingSocket::default();
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);
        let mut chunk_pacer = Pacer::new(OUTBOUND_CHUNKS_PER_SECOND, OUTBOUND_CHUNK_BURST);
        let mut direct_chunk_pacer = Pacer::new(DIRECT_CHUNKS_PER_SECOND, DIRECT_CHUNK_BURST);
        let leg = FakeReliableLeg::new(true);

        manager
            .history
            .insert(&NewMessage::outgoing_attachment(
                "m-1".into(),
                MessageKind::File,
                "a-1".into(),
                0,
                1,
            ))
            .unwrap();
        manager
            .history
            .upsert_attachment(&NewAttachment {
                id: "a-1".into(),
                kind: MessageKind::File,
                original_name: Some("big.bin".into()),
                mime: Some("application/octet-stream".into()),
                size: Some(size),
                sha256: Some(sha256.clone()),
                local_path: Some(source.to_string_lossy().to_string()),
                created_at: 0,
            })
            .unwrap();

        start_outgoing_transfer(
            &manager,
            &mut state,
            OutgoingRequest {
                transfer_id: 11,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "big.bin".into(),
                mime: "application/octet-stream".into(),
                size,
                sha256,
                path: source,
            },
            link::Route::Direct,
        )
        .unwrap();

        // 分片大小跟着 route 走：offer 里写的就是 48 KiB（接收侧按它算每一块）
        assert_eq!(
            state.transfers.get(&11).unwrap().chunk_size,
            P2P_CHUNK_SIZE as u32
        );

        flush(&mut socket, &mut state, &mut pacer, Some(&leg), false)
            .await
            .unwrap();

        assert!(socket.sent.is_empty(), "offer 不该经过服务器");
        assert_eq!(leg.frames().len(), 1);
        assert_eq!(leg.frames()[0][0], FrameKind::TransferControl.as_byte());

        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_ACCEPT,
                2,
                json!(TransferIdPayload { transfer_id: 11 }),
            ),
        )
        .unwrap();

        assert_eq!(
            state.transfers.get(&11).unwrap().phase,
            TransferPhase::Sending
        );

        // 背压翻假：一块都不发，返回 `Ok(false)`（不是错误——这一支靠等待再试）
        leg.block();

        assert!(
            !send_next_chunk(
                &mut socket,
                &manager,
                &mut state,
                &mut pacer,
                &mut chunk_pacer,
                &mut direct_chunk_pacer,
                Some(&leg),
            )
            .await
            .unwrap()
        );
        assert!(socket.sent.is_empty());
        assert_eq!(leg.frames().len(), 1, "背压时一块都不该上路");

        leg.unblock();

        // DC 的分片额度是这一支**真正**的门（不是中继那两个桶，也不是「有腿就放行」）：
        // 把 DC 的分片桶抽干，一块都发不出去，而且是 `Ok(false)` 而不是错误
        let mut drained = Pacer::new(DIRECT_CHUNKS_PER_SECOND, 0.0);

        assert!(
            !send_next_chunk(
                &mut socket,
                &manager,
                &mut state,
                &mut pacer,
                &mut chunk_pacer,
                &mut drained,
                Some(&leg),
            )
            .await
            .unwrap()
        );
        assert_eq!(leg.frames().len(), 1, "没有 DC 分片令牌时一块都不该上路");

        while send_next_chunk(
            &mut socket,
            &manager,
            &mut state,
            &mut pacer,
            &mut chunk_pacer,
            &mut direct_chunk_pacer,
            Some(&leg),
        )
        .await
        .unwrap()
        {}

        // 线格式全在腿上：offer + 3 块 + complete；中继那条线一条帧都没有
        assert!(socket.sent.is_empty(), "DC 那一单不该有任何帧经过服务器");
        assert_eq!(
            leg.frames().len(),
            1 + chunk_count(size, P2P_CHUNK_SIZE) as usize + 1
        );

        // 额度（R23 / R32）：中继的两个桶一枚都没被扣——DC 上的分片不经过中继的计费点
        assert!(
            (pacer.tokens - OUTBOUND_BURST).abs() < 0.05,
            "DC 那一单不该吃中继的通用额度: {}",
            pacer.tokens
        );
        assert!(
            (chunk_pacer.tokens - OUTBOUND_CHUNK_BURST).abs() < 0.05,
            "DC 那一单不该吃中继的分片额度: {}",
            chunk_pacer.tokens
        );

        // 分片拼回来必须和源文件逐字节一致（per-transfer 密钥）
        let cipher = PairCipher::new(&crypto::derive_transfer_key(&ROOT_KEY, 11));
        let frames = leg.frames();
        let mut rebuilt = Vec::new();

        for (index, frame) in frames[1..frames.len() - 1].iter().enumerate() {
            let (header, plain) = cipher.open(frame).unwrap();

            assert_eq!(header.kind, FrameKind::TransferChunk);
            assert_eq!(header.transfer_id, 11);
            assert_eq!(header.seq, index as u32);
            // 48 KiB 一块：最后一块是余数，其余都是整块，绝不是中继那个 512 KiB
            assert_eq!(
                plain.len(),
                if index + 1 == chunk_count(size, P2P_CHUNK_SIZE) as usize {
                    size as usize - index * P2P_CHUNK_SIZE
                } else {
                    P2P_CHUNK_SIZE
                }
            );

            rebuilt.extend_from_slice(&plain);
        }

        assert_eq!(rebuilt, payload);

        let (_, complete_plain) = PairCipher::new(&ROOT_KEY)
            .open(frames.last().unwrap())
            .unwrap();
        let complete = AppEnvelope::from_bytes(&complete_plain).unwrap();

        assert_eq!(complete.message_type, message_type::TRANSFER_COMPLETE);

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §43 / §10 第 4 条的落地：可靠那条腿不可用时，**钉在它上面的那一单**本地判失败，
    /// 并且在中继上补一条 `transfer.cancel`；钉在中继上的那一单一点都不受牵连。
    ///
    /// 这条直接调 [`direct_lost`]：会话层没有「真的拔掉一条 DC 腿」的入口（真拔腿只能在
    /// `p2p.rs` 的用例里做，而那层拿不到会话表），所以「收尾」这个动作本身只能钉在这里。
    /// `p2p.rs` 的 `dropping_the_leg_mid_burst_never_delivers_a_torn_chunk` 管的是另一半
    /// （拔腿时线上不会出现半块）。
    #[tokio::test]
    async fn losing_the_direct_leg_fails_its_transfer_and_cancels_it_over_the_relay() {
        let store = TransferStore::new(temp_root("direct-lost"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload: Vec<u8> = vec![9u8; 4096];
        let (source, size, sha256) = source_file(&root, &payload);

        for (message_id, attachment_id) in [("m-direct", "a-direct"), ("m-relay", "a-relay")] {
            manager
                .history
                .insert(&NewMessage::outgoing_attachment(
                    message_id.into(),
                    MessageKind::File,
                    attachment_id.into(),
                    0,
                    1,
                ))
                .unwrap();
            manager
                .history
                .upsert_attachment(&NewAttachment {
                    id: attachment_id.into(),
                    kind: MessageKind::File,
                    original_name: Some("x.bin".into()),
                    mime: Some("application/octet-stream".into()),
                    size: Some(size),
                    sha256: Some(sha256.clone()),
                    local_path: Some(source.to_string_lossy().to_string()),
                    created_at: 0,
                })
                .unwrap();
        }

        // 两条会话：一条钉在可靠腿上（DC 那一单），一条钉在中继上
        for (transfer_id, message_id, attachment_id, route) in [
            (71, "m-direct", "a-direct", link::Route::Direct),
            (72, "m-relay", "a-relay", link::Route::Relay),
        ] {
            start_outgoing_transfer(
                &manager,
                &mut state,
                OutgoingRequest {
                    transfer_id,
                    message_id: message_id.into(),
                    attachment_id: attachment_id.into(),
                    kind: TransferKind::File,
                    name: "x.bin".into(),
                    mime: "application/octet-stream".into(),
                    size,
                    sha256: sha256.clone(),
                    path: source.clone(),
                },
                route,
            )
            .unwrap();
        }

        assert_eq!(state.transfers.len(), 2);

        let mut socket = RecordingSocket::default();
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);

        // 先把两条 offer 推出去（都是中继那条线：没有 DC 腿）
        flush(&mut socket, &mut state, &mut pacer, None, false)
            .await
            .unwrap();

        assert_eq!(socket.sent.len(), 2, "两条 offer");

        socket.sent.clear();

        let tokens_before = pacer.tokens;

        direct_lost(&mut socket, &manager, 0, &mut state, &mut pacer)
            .await
            .unwrap();

        // DC 那一单：本地判失败 + 中继上补一条 cancel
        assert!(
            !state.transfers.contains_key(&71),
            "钉在 DC 上的那一单该被收掉"
        );
        assert_eq!(
            manager.history.find("m-direct").unwrap().unwrap().status,
            MessageStatus::Failed
        );

        assert_eq!(socket.sent.len(), 1, "cancel 只该有一条");

        let Message::Binary(frame) = &socket.sent[0] else {
            panic!("cancel 必须是二进制帧");
        };
        let (header, plain) = PairCipher::new(&ROOT_KEY).open(frame).unwrap();
        let cancel = AppEnvelope::from_bytes(&plain).unwrap();

        assert_eq!(header.kind, FrameKind::TransferControl);
        assert_eq!(cancel.message_type, message_type::TRANSFER_CANCEL);
        assert_eq!(cancel.payload["transferId"], json!(71));

        // 用户看到的那句话（`close_transfer` 的 reason）也要有断言钉着
        assert!(
            sink.payloads(EVENT_TRANSFER)
                .iter()
                .any(|payload| payload["transferId"] == 71 && payload["state"] == "failed"),
            "UI 该看到那一单失败：{:?}",
            sink.payloads(EVENT_TRANSFER)
        );
        assert!(
            sink.payloads(EVENT_ERROR)
                .iter()
                .any(|payload| payload["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("P2P 通路断开"))),
            "失败原因该报出来：{:?}",
            sink.payloads(EVENT_ERROR)
        );

        // cancel 走的是中继那条线：额度和 DC 那条腿无关
        assert!(
            (pacer.tokens - (tokens_before - 1.0)).abs() < 0.05,
            "cancel 该吃中继的帧额度：{} -> {}",
            tokens_before,
            pacer.tokens
        );

        // 中继那一单原封不动：DC 掉了不该牵连它
        let relay = state.transfers.get(&72).expect("中继那一单还在");

        assert_eq!(relay.route, link::Route::Relay);
        assert_eq!(relay.phase, TransferPhase::AwaitingAccept);
        // `Sent` 而不是 `Pending`：上面那条 offer 已经推出去了
        assert_eq!(
            manager.history.find("m-relay").unwrap().unwrap().status,
            MessageStatus::Sent
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §81：0 字节文件没有分片可发，`transfer.complete` 只能在收到 accept 时立刻补一条。
    /// 少了它两边会停在 sending / receiving，一直到断线才被判失败。
    #[tokio::test]
    async fn an_empty_file_completes_right_after_the_accept() {
        let store = TransferStore::new(temp_root("empty"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let (source, size, sha256) = source_file(&root, &[]);
        let empty_sha256 = sha256.clone();

        assert_eq!(size, 0);
        assert_eq!(chunk_count(size, CHUNK_SIZE), 0);

        manager
            .history
            .insert(&NewMessage::outgoing_attachment(
                "m-1".into(),
                MessageKind::File,
                "a-1".into(),
                0,
                1,
            ))
            .unwrap();
        manager
            .history
            .upsert_attachment(&NewAttachment {
                id: "a-1".into(),
                kind: MessageKind::File,
                original_name: Some("empty.bin".into()),
                mime: Some("application/octet-stream".into()),
                size: Some(size),
                sha256: Some(sha256.clone()),
                local_path: Some(source.to_string_lossy().to_string()),
                created_at: 0,
            })
            .unwrap();

        start_outgoing_transfer(
            &manager,
            &mut state,
            OutgoingRequest {
                transfer_id: 7,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "empty.bin".into(),
                mime: "application/octet-stream".into(),
                size,
                sha256,
                path: source,
            },
            link::Route::Relay,
        )
        .unwrap();

        let mut socket = RecordingSocket::default();
        let mut pacer = Pacer::new(OUTBOUND_FRAMES_PER_SECOND, OUTBOUND_BURST);

        flush(&mut socket, &mut state, &mut pacer, None, false)
            .await
            .unwrap();

        assert!(!state.has_pending_chunks(), "空文件不该有待发分片");

        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_ACCEPT,
                2,
                json!(TransferIdPayload { transfer_id: 7 }),
            ),
        )
        .unwrap()
        .expect("accept 之后必须立刻补一条 transfer.complete");

        assert_eq!(reply.0, FrameKind::TransferControl);
        assert_eq!(reply.1.message_type, message_type::TRANSFER_COMPLETE);

        // 接收方那一侧：0 分片的 offer（不是大文件，自动接收）收到 complete 必须能收尾
        let receiver_store = TransferStore::new(temp_root("empty-receive"));
        let (receiver, _receiver_sink, receiver_root) = manager_with_store(receiver_store);
        let mut receiver_state = SessionState::new(&ROOT_KEY);

        let accepted = deliver(
            &receiver,
            &mut receiver_state,
            FrameKind::TransferControl,
            &offer_for(21, TransferKind::File, "empty.bin", 0, &empty_sha256),
        )
        .unwrap();

        assert!(accepted.is_some(), "0 字节不是大文件，应当自动接收并回 accept");

        let verified = deliver(
            &receiver,
            &mut receiver_state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_COMPLETE,
                3,
                json!(TransferIdPayload { transfer_id: 21 }),
            ),
        )
        .unwrap()
        .expect("0 分片的 complete 必须能收尾");

        assert_eq!(verified.1.message_type, message_type::TRANSFER_VERIFIED);
        assert_eq!(verified.1.payload["ok"], true);
        assert!(receiver_state.transfers.is_empty(), "接收方会话要摘掉");
        assert_eq!(
            receiver.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Received
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&receiver_root).unwrap();
    }

    /// §7 / R32：分片大小由**对端**给，所以不能只认本机常量；但必须在合法范围内、且和
    /// `size` 对得上。不合法就回一条 reject——以前那种「只在本机 `emit_error`」会让发送方
    /// 一直停在 `AwaitingAccept` 等一个永远不来的回执。
    #[tokio::test]
    async fn an_offer_with_an_impossible_chunk_size_is_rejected() {
        let store = TransferStore::new(temp_root("bad-chunk"));
        let (manager, _sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        // 块边界两侧各有一块：中继 2 块、P2P 11 块，这样「块数和大小对不上」才真的对不上
        let (_source, size, sha256) = source_file(&root, &vec![7u8; CHUNK_SIZE + 7]);

        // 比下界还小 / 直接给 0：对端可以拿它逼你在本地写一堆小文件
        for chunk_size in [MIN_CHUNK_SIZE - 1, 0] {
            let reply = deliver(
                &manager,
                &mut state,
                FrameKind::TransferControl,
                &offer_with_chunk_size(31, TransferKind::File, "x.bin", size, &sha256, chunk_size),
            )
            .unwrap()
            .expect("不合法的分片大小必须回一条 reject");

            assert_eq!(reply.1.message_type, message_type::TRANSFER_REJECT);
            assert!(state.transfers.is_empty(), "不合法不该建会话");
        }

        // 大小合法但 `chunks` 和 `size` 对不上（拿 512 KiB 的块数报 48 KiB 的大小）
        let inconsistent = AppEnvelope::new(
            message_type::TRANSFER_OFFER,
            1,
            json!(TransferOfferPayload {
                transfer_id: 32,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "x.bin".into(),
                size,
                mime: "application/octet-stream".into(),
                sha256: sha256.clone(),
                chunk_size: P2P_CHUNK_SIZE as u32,
                chunks: chunk_count(size, CHUNK_SIZE),
            }),
        );

        let reply = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &inconsistent,
        )
        .unwrap()
        .expect("块数对不上必须回一条 reject");

        assert_eq!(reply.1.message_type, message_type::TRANSFER_REJECT);
        assert!(state.transfers.is_empty());

        // 48 KiB 是合法的一档（Phase 10 的 DC 那一单用的就是它），中继入站的 offer 钉中继
        let accepted = deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_with_chunk_size(
                33,
                TransferKind::File,
                "x.bin",
                size,
                &sha256,
                P2P_CHUNK_SIZE,
            ),
        )
        .unwrap();

        assert!(accepted.is_some(), "48 KiB 的 offer 应当被接受");

        let session = state.transfers.get(&33).unwrap();

        assert_eq!(session.chunk_size, P2P_CHUNK_SIZE as u32);
        assert_eq!(session.route, link::Route::Relay, "从哪条 lane 来就钉哪条");
        assert_eq!(session.chunks, chunk_count(size, P2P_CHUNK_SIZE));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §81：对方拒绝时，发送方要标记失败并说明原因
    #[tokio::test]
    async fn a_reject_marks_the_outgoing_attachment_failed() {
        let store = TransferStore::new(temp_root("reject"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let (source, size, sha256) = source_file(&root, &vec![9u8; 128]);

        manager
            .history
            .insert(&NewMessage::outgoing_attachment(
                "m-1".into(),
                MessageKind::File,
                "a-1".into(),
                0,
                1,
            ))
            .unwrap();

        start_outgoing_transfer(
            &manager,
            &mut state,
            OutgoingRequest {
                transfer_id: 6,
                message_id: "m-1".into(),
                attachment_id: "a-1".into(),
                kind: TransferKind::File,
                name: "source.bin".into(),
                mime: "application/octet-stream".into(),
                size,
                sha256,
                path: source,
            },
            link::Route::Relay,
        )
        .unwrap();

        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &AppEnvelope::new(
                message_type::TRANSFER_REJECT,
                2,
                json!(TransferRejectPayload {
                    transfer_id: 6,
                    reason: "超过本机允许的附件大小".to_string(),
                }),
            ),
        )
        .unwrap();

        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Failed
        );
        assert!(state.transfers.is_empty());
        assert!(
            sink.payloads(EVENT_TRANSFER)
                .iter()
                .any(|payload| payload["state"] == "failed"
                    && payload["percent"] == 0
                    && payload["direction"] == "outgoing")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// §81：传输中断线 —— 半成品要删掉、消息标记失败、还能重试
    #[test]
    fn a_disconnect_mid_transfer_cleans_up_and_allows_a_retry() {
        let store = TransferStore::new(temp_root("interrupt"));
        let (manager, sink, root) = manager_with_store(store);
        let mut state = SessionState::new(&ROOT_KEY);

        let payload: Vec<u8> = vec![5u8; CHUNK_SIZE + 1];
        let (_, size, sha256) = source_file(&root, &payload);

        deliver(
            &manager,
            &mut state,
            FrameKind::TransferControl,
            &offer_for(21, TransferKind::Image, "cat.png", size, &sha256),
        )
        .unwrap();
        deliver_chunk(&manager, &mut state, 21, 0, &payload[..CHUNK_SIZE]).unwrap();

        assert_eq!(count_part_files(&root), 1);

        abort_transfers(&manager, &mut state);

        assert!(state.transfers.is_empty());
        assert_eq!(count_part_files(&root), 0, "断线要删掉 .part");
        assert_eq!(
            manager.history.find("m-1").unwrap().unwrap().status,
            MessageStatus::Failed
        );
        assert!(
            sink.payloads(EVENT_TRANSFER)
                .iter()
                .any(|payload| payload["state"] == "failed"
                    && payload["message"] == "连接断开，传输已中断")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 一个**不是** WebSocket 的传输替身。
    ///
    /// Phase 8a 的全部意义就是「`live` 只依赖 Sink + Stream 抽象」：Phase 8b 的
    /// DataChannel 适配器要按同一组约束接进来。这个替身让下面那条用例真的把 `live`
    /// 跑在一根不是 WebSocket 的传输上——哪天有人把 `live` 重新绑回 `PairSocket`，
    /// 它会先编译不过。
    struct FakeTransport {
        inbound: mpsc::UnboundedReceiver<Message>,
        sent: Arc<Mutex<Vec<Message>>>,
    }

    impl futures_util::Stream for FakeTransport {
        type Item = Result<Message, std::convert::Infallible>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.inbound.poll_recv(cx).map(|message| message.map(Ok))
        }
    }

    impl futures_util::Sink<Message> for FakeTransport {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.sent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(item);

            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// 等一个条件成立；超时返回它最后一次的取值，方便断言里看到真实状态
    async fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while tokio::time::Instant::now() < deadline {
            if predicate() {
                return true;
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        predicate()
    }

    /// Phase 8a：`live` 跑在一条不是 WebSocket 的传输上，入站与出站都照常工作。
    #[tokio::test]
    async fn live_runs_over_a_transport_that_is_not_a_websocket() {
        let (manager, sink) = test_manager();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let transport = FakeTransport {
            inbound: inbound_rx,
            sent: Arc::clone(&sent),
        };
        let mut receiver = with_session(&manager);
        let mut state = SessionState::new(&ROOT_KEY);

        let driver = {
            let manager = Arc::clone(&manager);

            tokio::spawn(
                async move { live(&manager, 0, &mut state, transport, &mut receiver).await },
            )
        };

        // 入站：中继握手帧（文本控制帧，和 WebSocket 无关）
        inbound_tx
            .send(Message::text(
                json!({
                    "type": "server.welcome",
                    "protocol": PROTOCOL_VERSION,
                    "peerOnline": true,
                })
                .to_string(),
            ))
            .unwrap();

        assert!(
            wait_until(|| manager.status().peer_online).await,
            "welcome 没有被应用：{:?}",
            manager.status()
        );

        // 出站：一条应用消息要先过 Pacer，再写成二进制帧
        manager
            .send(FrameKind::Ping, message_type::PING, json!({ "sentAt": 1 }))
            .unwrap();

        let written = || {
            sent.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|message| matches!(message, Message::Binary(_)))
        };

        assert!(wait_until(written).await, "假传输上没有写出任何应用帧");

        // 传输结束 → 会话按「中继关闭了连接」收尾（而不是被用户停掉）
        inbound_tx.send(Message::Close(None)).unwrap();

        match driver.await.unwrap() {
            Outcome::Lost(failure) => {
                assert_eq!(failure.message, "中继关闭了连接");
                assert!(!failure.fatal);
            }
            Outcome::Stopped => panic!("不该是被用户停掉"),
        }

        assert!(!sink.payloads(EVENT_CONNECTION_CHANGED).is_empty());
    }
}

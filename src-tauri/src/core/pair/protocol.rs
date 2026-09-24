//! 双人联机的协议定义：明文帧头、应用层信封、服务端控制帧。
//!
//! 这里只有数据结构与编解码，不含连接、UI 或业务逻辑。帧头格式必须与
//! `server-cloudflare/src/protocol.ts` 保持一致。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

pub const PROTOCOL_VERSION: u8 = 1;

/// 每个应用帧固定 14 字节明文帧头：kind(1) | flags(1) | transferId(8) | seq(4)
pub const FRAME_HEADER_SIZE: usize = 14;

/// XChaCha20-Poly1305 nonce 长度
pub const NONCE_SIZE: usize = 24;

/// 单帧上限（整帧，含帧头与 nonce/tag），与服务端一致
pub const MAX_BINARY_FRAME_SIZE: usize = 1024 * 1024;

/// 明文帧头里的 kind。中继只读它做分桶限流，所以取值必须与中继一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    PetState = 1,
    Presence = 2,
    Stats = 3,
    Chat = 4,
    TransferControl = 5,
    TransferChunk = 6,
    Ack = 7,
    Ping = 8,
}

impl FrameKind {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::PetState),
            2 => Some(Self::Presence),
            3 => Some(Self::Stats),
            4 => Some(Self::Chat),
            5 => Some(Self::TransferControl),
            6 => Some(Self::TransferChunk),
            7 => Some(Self::Ack),
            8 => Some(Self::Ping),
            _ => None,
        }
    }
}

/// 明文帧头。同时也是 AEAD 的 associated data，改动任何字段都会导致解密失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: FrameKind,
    pub flags: u8,
    pub transfer_id: u64,
    pub seq: u32,
}

impl FrameHeader {
    pub const fn new(kind: FrameKind, seq: u32) -> Self {
        Self {
            kind,
            flags: 0,
            transfer_id: 0,
            seq,
        }
    }

    pub fn encode(self) -> [u8; FRAME_HEADER_SIZE] {
        let mut bytes = [0u8; FRAME_HEADER_SIZE];

        bytes[0] = self.kind.as_byte();
        bytes[1] = self.flags;
        bytes[2..10].copy_from_slice(&self.transfer_id.to_be_bytes());
        bytes[10..14].copy_from_slice(&self.seq.to_be_bytes());

        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FRAME_HEADER_SIZE {
            return None;
        }

        let kind = FrameKind::from_byte(bytes[0])?;
        let flags = bytes[1];
        let transfer_id = u64::from_be_bytes(bytes[2..10].try_into().ok()?);
        let seq = u32::from_be_bytes(bytes[10..14].try_into().ok()?);

        Some(Self {
            kind,
            flags,
            transfer_id,
            seq,
        })
    }
}

/// 应用层消息的 `type` 取值
pub mod message_type {
    pub const PING: &str = "pair.ping";
    pub const PONG: &str = "pair.pong";
    /// P2P 信令（R21）。走 `FrameKind::Ping`(8)，靠 `type` 与 `pair.ping` 区分。
    pub const SIGNAL: &str = "pair.signal";
    pub const PRESENCE: &str = "pair.presence";
    pub const PET_STATE: &str = "pair.pet-state";
    pub const STATS: &str = "pair.stats";
    pub const CHAT_TEXT: &str = "chat.text";
    pub const CHAT_ACK: &str = "chat.ack";
    pub const TRANSFER_OFFER: &str = "transfer.offer";
    pub const TRANSFER_ACCEPT: &str = "transfer.accept";
    pub const TRANSFER_REJECT: &str = "transfer.reject";
    pub const TRANSFER_COMPLETE: &str = "transfer.complete";
    pub const TRANSFER_VERIFIED: &str = "transfer.verified";
    pub const TRANSFER_CANCEL: &str = "transfer.cancel";
}

/// P2P 信令的版本（R21）。不认识这个版本就不协商——将来改信令形状时靠它挡住。
pub const SIGNAL_VERSION: u8 = 1;

/// 对端声明「支持第二条 DataChannel（`reliable`）」时 `hello` 里带的能力名（R32）。
///
/// **不能用 bump `SIGNAL_VERSION` 代替**：那个版本是硬相等判断（不匹配就完全不协商），
/// 一提就会把「新客户端 ↔ 旧客户端」之间**连 `pet-state` 的 P2P** 一起关掉。能力字段是
/// 可选的，缺失就表示老客户端——它只会建 / 认领 `pet-state`。
pub const FEATURE_RELIABLE_CHANNEL: &str = "reliable-channel";

/// `pair.signal` 的载荷（R21）。
///
/// 走 `FrameKind::Ping`(8)：中继会校验帧 kind，未知值直接 `close 1008`，而 kind 8
/// 早就在它的已知集合里，所以旧中继照样原样转发。四种消息：
///
/// - `hello`：双方各自宣告「我支持 P2P」。**能力门控只看对端**——没收到对端的 hello
///   就不发起 ICE，否则对面是旧客户端时会白等一轮超时。里面的 `deviceId` 还兼作
///   glare 的裁决：**字典序小的一方发起 offer**，避免双方同时 offer。
/// - `offer` / `answer`：`description` 是序列化后的 `RTCSessionDescription`（里面既有
///   SDP 文本也有类型），不是裸 SDP——只发 SDP 文本会丢掉 offer/answer 的类型。
/// - `candidate`：ICE candidate，trickle 发送。
///
/// 字段名用 camelCase，和协议里其它 JSON 一致；这里逐字段写 `rename` 而不是靠
/// `rename_all_fields`，免得依赖 serde 的较新版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PairSignalPayload {
    Hello {
        version: u8,
        #[serde(rename = "deviceId")]
        device_id: String,
        /// 可选能力（R32）：缺失 / 空 = 旧客户端。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    Offer {
        description: String,
    },
    Answer {
        description: String,
    },
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid", default, skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,
        #[serde(
            rename = "sdpMLineIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        sdp_mline_index: Option<u16>,
    },
}

/// 应用层信封（加密前的内容）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEnvelope {
    pub v: u8,
    pub id: String,
    pub seq: u64,
    pub sent_at: i64,
    #[serde(rename = "type")]
    pub message_type: String,
    pub payload: Value,
}

impl AppEnvelope {
    pub fn new(message_type: &str, seq: u64, payload: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            seq,
            sent_at: now_millis(),
            message_type: message_type.to_string(),
            payload,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|err| format!("序列化消息失败: {err}"))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|err| format!("解析消息失败: {err}"))
    }
}

pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceState {
    Active,
    Away,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresencePayload {
    pub state: PresenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// `chat.text` 的载荷（§31）。`message_id` 由发送方生成，也是本地库里的主键。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTextPayload {
    pub message_id: String,
    pub text: String,
}

/// `chat.ack` 的载荷：只说「哪条消息收到了」，不回到信
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAckPayload {
    pub message_id: String,
}

/// 附件类型（§37 / §38 / §44）。落地成本地消息时映射成 `MessageKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    Image,
    File,
    Voice,
}

impl TransferKind {
    /// 线上字符串；目前只有测试与日志会用到，主流程用的是 serde 的 lowercase 映射
    #[allow(dead_code)]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
            Self::Voice => "voice",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "image" => Ok(Self::Image),
            "file" => Ok(Self::File),
            "voice" => Ok(Self::Voice),
            other => Err(format!("未知的附件类型: {other}")),
        }
    }
}

/// `transfer.offer`（§39）：只带文件名与校验信息。
///
/// 发送方的本地完整路径**永远不出现**在载荷里（§39 / §78），接收方落盘用的是自己生成的 UUID。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferOfferPayload {
    pub transfer_id: u64,
    /// 与 `chat.text` 一样，收发双方用同一个 id 落地成一条消息
    pub message_id: String,
    /// 附件 id 也由发送方生成：重发时两端都覆盖同一条附件记录
    pub attachment_id: String,
    pub kind: TransferKind,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub sha256: String,
    pub chunk_size: u32,
    pub chunks: u32,
}

/// `transfer.accept` / `transfer.complete` / `transfer.cancel` 的载荷
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferIdPayload {
    pub transfer_id: u64,
}

/// `transfer.reject`：接收方不接受（磁盘、上限或用户点了拒绝）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRejectPayload {
    pub transfer_id: u64,
    pub reason: String,
}

/// `transfer.verified`：接收方校验完 SHA-256 后告诉发送方结果（§38 / §41）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferVerifiedPayload {
    pub transfer_id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 键盘活动：哪只手 + 强度 + **当前按着的键名**（R37）。
///
/// R2 / R3 那部分（左右手、强度）口径不变；`keys` 是用户明确要求的改动：不再只发
/// 「哪只手」，也把键名发出去，这样对方的猫能按下一样的键。仍然有边界——只带本机模型
/// 真的能显示的键、单个名字限长、总数限 8 个，且收发两侧都要过 [`sanitize_keys`]。
///
/// 旧客户端发来的载荷没有这个字段（`default`），新客户端发给旧客户端时对方会忽略它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetKeyboardState {
    pub active: bool,
    pub left_hand: bool,
    pub right_hand: bool,
    /// 0..1，发送前量化到 0.2
    pub intensity: f32,
    /// rdev 的原始键名（`KeyA` / `ShiftLeft`），去重 + 排序 + 上限 [`KEY_LIST_MAX`]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// 鼠标活动：位置是屏幕比例（0..1），永远不含真实像素坐标
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetPointerState {
    pub active: bool,
    /// 0..1 屏幕比例，发送前量化到 0.02
    pub x: f32,
    pub y: f32,
    /// 0..1 归一化移动速度
    pub speed: f32,
    pub left_down: bool,
    pub right_down: bool,
}

/// 远端宠物快照（§16）。这是唯一通过网络传输的「活动」结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetSnapshot {
    pub keyboard: PetKeyboardState,
    pub pointer: PetPointerState,
}

impl Default for PetSnapshot {
    fn default() -> Self {
        Self {
            keyboard: PetKeyboardState {
                active: false,
                left_hand: false,
                right_hand: false,
                intensity: 0.0,
                keys: Vec::new(),
            },
            pointer: PetPointerState {
                active: false,
                x: 0.5,
                y: 0.5,
                speed: 0.0,
                left_down: false,
                right_down: false,
            },
        }
    }
}

impl PetSnapshot {
    /// 收发两侧都跑一遍：NaN → 0，越界裁到 0..1，并按 R4 的量化步长取整。
    ///
    /// 对端是本该受信任的配对方，但这层仍然必要：它保证 UI 只会拿到有界的、
    /// 量化过的值（不会有真实比例的高精度信息，也不会有能让动画炸掉的 NaN）。
    pub fn sanitized(self) -> Self {
        Self {
            keyboard: PetKeyboardState {
                active: self.keyboard.active,
                left_hand: self.keyboard.left_hand,
                right_hand: self.keyboard.right_hand,
                intensity: quantize(self.keyboard.intensity, 0.2),
                keys: sanitize_keys(self.keyboard.keys),
            },
            pointer: PetPointerState {
                active: self.pointer.active,
                x: quantize(self.pointer.x, 0.02),
                y: quantize(self.pointer.y, 0.02),
                speed: quantize(self.pointer.speed, 0.05),
                left_down: self.pointer.left_down,
                right_down: self.pointer.right_down,
            },
        }
    }
}

/// 输入统计（§24 / §25）。
///
/// 计数值是「键盘按下次数」与「鼠标按键按下次数」，按物理键名去重（R7），
/// 不包含任何按键内容。`share` 是相对 §25 的示例载荷多出来的一个布尔：关闭分享时
/// 对端仍然需要知道「对方现在不分享统计」，否则会一直显示上一次的旧数字。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputStats {
    /// 本地日期 yyyy-mm-dd，由拥有本地时区的前端提供
    pub date: String,
    pub today_keyboard: u64,
    pub today_mouse: u64,
    pub total_keyboard: u64,
    pub total_mouse: u64,
    pub share: bool,
}

/// 裁到 0..1 并量化到 step 的整数倍
fn quantize(value: f32, step: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }

    let clamped = value.clamp(0.0, 1.0);

    (clamped / step).round() * step
}

/// 键名列表的上限（R37）。与 `usePairActivity.ts` 的 `KEY_LIST_MAX` 同值：
/// 前端先筛一遍，这里再兜一遍，两边都不会让载荷无限长。
pub const KEY_LIST_MAX: usize = 8;
/// 单个键名的长度上限。rdev 最长的名字（`IntlBackslash`、`Unknown(255)`）都在它之内。
pub const KEY_NAME_MAX: usize = 24;

/// 这个字符是否允许出现在键名里（R37）。
///
/// rdev 的键名是枚举名（`KeyA`、`Num1`、`ShiftLeft`、`Unknown(255)`），所以只需要
/// 字母 + 数字 + 括号 + 下划线；别的字符一律当成畸形载荷丢掉。
fn is_key_name_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '(' | ')' | '_')
}

/// 键名列表的兜底（R37）：丢掉不像键名的字符串、去重、排序、截断到 [`KEY_LIST_MAX`]。
///
/// 排序是为了让「同一组键」序列化出来完全一致：前端 R4 的「没变化就不发」用的是深比较，
/// 顺序不稳定会让每一帧都被当成变化。前端 `sanitizeKeys` 是同口径的另一半。
pub fn sanitize_keys(keys: Vec<String>) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();

    for key in keys {
        if key.is_empty() || key.chars().count() > KEY_NAME_MAX {
            continue;
        }

        if !key.chars().all(is_key_name_char) {
            continue;
        }

        if clean.iter().any(|existing| existing == &key) {
            continue;
        }

        clean.push(key);
    }

    clean.sort();
    clean.truncate(KEY_LIST_MAX);
    clean
}

/// 中继可选广告的 ICE 服务器（R21）。
///
/// 形状跟 WebRTC 的 `RTCIceServer` 一致：`urls` 可以是单个字符串也可以是字符串数组，
/// `username` / `credential` 是 TURN 的静态凭据。
///
/// 默认**不填**任何公共 STUN——STUN 必然让第三方看到公网 IP，而 README 承诺不收集
/// 任何用户数据。缺失时只有 host candidate（同局域网可用），这正是隐私默认。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    #[serde(deserialize_with = "deserialize_ice_urls")]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

/// `urls` 在 WebRTC 里既可以是单个字符串也可以是数组，两种都收
fn deserialize_ice_urls<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(url) => vec![url],
        OneOrMany::Many(urls) => urls,
    })
}

/// 解析中继广告的 ICE 服务器。缺字段、`null`、形状不对都按「没广告」处理；数组里
/// 坏掉的条目逐条丢掉，好过整份丢掉（与 `deserialize_limits` 的逐字段判定同思路）。
///
/// 中继是对方维护的，不能让它把客户端推进一个坏状态；空列表是安全的缺省值。
fn deserialize_ice_servers<'de, D>(deserializer: D) -> Result<Vec<IceServer>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    let Some(Value::Array(entries)) = value else {
        return Ok(Vec::new());
    };

    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<IceServer>(entry).ok())
        .collect())
}

/// 中继发来的明文控制帧
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    #[serde(rename = "server.welcome")]
    Welcome {
        protocol: u8,
        #[serde(rename = "peerOnline")]
        peer_online: bool,
        /// R20：自建中继会广告自己的限流额度；旧中继没有这个字段，
        /// 畸形或不合常理的额度按「没广告」处理（见 `deserialize_limits`）。
        #[serde(default, deserialize_with = "deserialize_limits")]
        limits: Option<RelayLimits>,
        /// R21：自建中继配了 coturn 才会广告；缺字段 / 畸形都按空处理
        /// （见 `deserialize_ice_servers`）。
        #[serde(
            rename = "iceServers",
            default,
            deserialize_with = "deserialize_ice_servers"
        )]
        ice_servers: Vec<IceServer>,
    },
    #[serde(rename = "server.peer")]
    Peer {
        online: bool,
        #[serde(rename = "deviceId")]
        device_id: String,
    },
    #[serde(rename = "server.error")]
    Error { code: String, message: String },
}

/// 中继在 `server.welcome` 里广告的限流额度（R20）。
///
/// 旧中继（例如已经部署的 Cloudflare 版）不带这个字段，所以它永远是可选的；
/// 缺失时按 CF 的缺省值 30 / 20 / 12 MiB 推导客户端自己的出站额度。
///
/// 三个维度**各自独立**判定：某一个字段坏掉只让它退回缺省，不会把另外两个一起丢掉。
/// 超过上限的值会被夹住——中继是对方维护的，一个出错或恶意的中继不该能把客户端的
/// 出站速率推到任意高。
///
/// 客户端目前只消费 `framesPerSecond` 与 `chunksPerSecond`：`bytesPerSecond` 是中继
/// 自己的字节桶（保护它不被大帧打爆），而客户端的字节速率由「分片大小 × 分片速率」
/// 决定，本来就低于它。
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayLimits {
    pub frames_per_second: f64,
    pub chunks_per_second: f64,
    pub bytes_per_second: f64,
}

impl RelayLimits {
    /// CF 版（也是旧中继）的缺省额度：没有广告值时用它
    pub const fn cloudflare() -> Self {
        Self {
            frames_per_second: 30.0,
            chunks_per_second: 20.0,
            bytes_per_second: 12.0 * 1024.0 * 1024.0,
        }
    }

    /// 客户端自己的出站额度。
    ///
    /// R20：**不 1:1 取用**广告值，而是按今天的比例留出余量——帧取 2/3
    /// （CF 30 → 20）、分片取 3/4（20 → 15）、分片突发取 1/2（20 → 10）。
    /// 贴死上限时，任何到达间隔抖动都会把中继的令牌桶扣穿、被 `close 1008`。
    pub fn outbound(self) -> OutboundLimits {
        // 先乘后除：30 * 2 / 3 正好是 20，不会因为浮点误差变成 19.999…
        let frames_per_second =
            self.frames_per_second * FRAME_RATE_NUMERATOR / FRAME_RATE_DENOMINATOR;
        let chunks_per_second =
            self.chunks_per_second * CHUNK_RATE_NUMERATOR / CHUNK_RATE_DENOMINATOR;

        OutboundLimits {
            // 下限 1/秒：广告值再离谱也不能推成 0（0 会让等待时长除零、永远发不出帧）
            frames_per_second: frames_per_second.max(1.0),
            frames_burst: frames_per_second.max(1.0),
            chunks_per_second: chunks_per_second.max(1.0),
            chunks_burst: (self.chunks_per_second * CHUNK_BURST_NUMERATOR
                / CHUNK_BURST_DENOMINATOR)
                .max(1.0),
        }
    }
}

/// 帧额度取广告值的 2/3（CF 30 → 20）
const FRAME_RATE_NUMERATOR: f64 = 2.0;
const FRAME_RATE_DENOMINATOR: f64 = 3.0;
/// 分片额度取广告值的 3/4（CF 20 → 15）
const CHUNK_RATE_NUMERATOR: f64 = 3.0;
const CHUNK_RATE_DENOMINATOR: f64 = 4.0;
/// 分片突发取广告值的 1/2（CF 20 → 10）
const CHUNK_BURST_NUMERATOR: f64 = 1.0;
const CHUNK_BURST_DENOMINATOR: f64 = 2.0;

/// 采信广告值的上限：再高也不认（中继是对方维护的，不能被它推成任意速率）
const MAX_ADVERTISED_FRAMES_PER_SECOND: f64 = 240.0;
const MAX_ADVERTISED_CHUNKS_PER_SECOND: f64 = 240.0;
const MAX_ADVERTISED_BYTES_PER_SECOND: f64 = 64.0 * 1024.0 * 1024.0;

/// `server.welcome` 带来的运行期配置（R20 / R21）。
///
/// 两个字段都可能缺失：旧中继不广告 `limits`（按 CF 缺省推导），也不广告
/// `iceServers`（只有 host candidate）。会话层拿到的是「这次连接的有效配置」，
/// 永远是确定的。
#[derive(Debug, Clone, PartialEq)]
pub struct RelayConfig {
    pub limits: RelayLimits,
    pub ice_servers: Vec<IceServer>,
}

/// 客户端实际使用的出站节奏（见 `RelayLimits::outbound`）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutboundLimits {
    pub frames_per_second: f64,
    pub frames_burst: f64,
    pub chunks_per_second: f64,
    pub chunks_burst: f64,
}

/// 容忍畸形的 `limits`：先当普通 JSON 读出来，解析失败或不合理就当作「没广告」。
///
/// 直接用 `Option<RelayLimits>` 的话，一个坏字段（例如 `framesPerSecond: "x"`）会让
/// 整条 `server.welcome` 变成「无法解析」，连 `peerOnline` 一起丢掉。
fn deserialize_limits<'de, D>(deserializer: D) -> Result<Option<RelayLimits>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let fallback = RelayLimits::cloudflare();

    Ok(Some(RelayLimits {
        frames_per_second: advertised(
            object.get("framesPerSecond"),
            fallback.frames_per_second,
            MAX_ADVERTISED_FRAMES_PER_SECOND,
        ),
        chunks_per_second: advertised(
            object.get("chunksPerSecond"),
            fallback.chunks_per_second,
            MAX_ADVERTISED_CHUNKS_PER_SECOND,
        ),
        bytes_per_second: advertised(
            object.get("bytesPerSecond"),
            fallback.bytes_per_second,
            MAX_ADVERTISED_BYTES_PER_SECOND,
        ),
    }))
}

/// 取一个维度：缺失 / 非数字 / 非正的有限数都退回缺省，超过上限就夹住
fn advertised(value: Option<&Value>, fallback: f64, max: f64) -> f64 {
    match value.and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value > 0.0 => value.min(max),
        _ => fallback,
    }
}

/// 最近处理过的消息 id，用于丢弃重连/重发带来的重复消息
pub struct RecentMessageIds {
    capacity: usize,
    ids: VecDeque<String>,
}

impl RecentMessageIds {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ids: VecDeque::with_capacity(capacity),
        }
    }

    /// 返回 true 表示这是新消息，false 表示重复
    pub fn insert(&mut self, id: &str) -> bool {
        if self.ids.iter().any(|item| item == id) {
            return false;
        }

        if self.ids.len() >= self.capacity {
            self.ids.pop_front();
        }

        self.ids.push_back(id.to_string());

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let header = FrameHeader {
            kind: FrameKind::TransferChunk,
            flags: 3,
            transfer_id: 0x0102_0304_0506_0708,
            seq: 42,
        };

        let encoded = header.encode();

        assert_eq!(encoded.len(), FRAME_HEADER_SIZE);
        assert_eq!(FrameHeader::decode(&encoded), Some(header));
    }

    #[test]
    fn header_rejects_short_or_unknown_kind() {
        assert_eq!(FrameHeader::decode(&[0u8; FRAME_HEADER_SIZE - 1]), None);

        let mut bytes = [0u8; FRAME_HEADER_SIZE];
        bytes[0] = 99;

        assert_eq!(FrameHeader::decode(&bytes), None);
    }

    #[test]
    fn recent_ids_drop_duplicates_and_evict_oldest() {
        let mut recent = RecentMessageIds::new(2);

        assert!(recent.insert("a"));
        assert!(!recent.insert("a"));
        assert!(recent.insert("b"));
        assert!(recent.insert("c"));

        // "a" 已经被挤出去，可以被当成新消息
        assert!(recent.insert("a"));
    }

    #[test]
    fn envelope_round_trip() {
        let envelope = AppEnvelope::new(
            message_type::PRESENCE,
            7,
            serde_json::json!({ "state": "away", "message": "去吃饭啦" }),
        );

        let bytes = envelope.to_bytes().unwrap();
        let parsed = AppEnvelope::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.message_type, message_type::PRESENCE);
        assert_eq!(parsed.payload["state"], "away");
    }

    /// R20：自建中继会广告限流额度；旧中继没有这个字段；坏值不影响其余字段
    #[test]
    fn welcome_limits_are_optional_and_tolerant() {
        let advertised: ServerFrame = serde_json::from_str(
            r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":{"framesPerSecond":60,"chunksPerSecond":20,"bytesPerSecond":12582912}}"#,
        )
        .unwrap();

        let ServerFrame::Welcome {
            limits: Some(limits),
            ..
        } = advertised
        else {
            panic!("应当解析出 limits");
        };

        assert_eq!(limits.frames_per_second, 60.0);
        assert_eq!(limits.chunks_per_second, 20.0);
        assert_eq!(limits.bytes_per_second, 12.0 * 1024.0 * 1024.0);

        // 畸形的字段不能让整条 welcome 解析失败，也不能影响 peerOnline；坏字段只让它
        // 自己退回缺省，不会连带丢掉另外两个维度
        let defaults = RelayLimits::cloudflare();

        for (bad, frames, chunks, bytes) in [
            // 只有 framesPerSecond 坏掉：另外两维照常保留
            (
                r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":{"framesPerSecond":"x"}}"#,
                defaults.frames_per_second,
                defaults.chunks_per_second,
                defaults.bytes_per_second,
            ),
            // framesPerSecond 是 0（非正）→ 退回缺省；bytesPerSecond 是 1 → 照常保留
            (
                r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":{"framesPerSecond":0,"chunksPerSecond":20,"bytesPerSecond":1}}"#,
                defaults.frames_per_second,
                20.0,
                1.0,
            ),
            // 空对象：三维全部退回缺省
            (
                r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":{}}"#,
                defaults.frames_per_second,
                defaults.chunks_per_second,
                defaults.bytes_per_second,
            ),
        ] {
            let ServerFrame::Welcome {
                protocol,
                peer_online,
                limits,
                ..
            } = serde_json::from_str::<ServerFrame>(bad).unwrap()
            else {
                panic!("应当仍然是 welcome: {bad}");
            };

            assert_eq!(protocol, 1, "{bad}");
            assert!(peer_online, "{bad}");

            let limits = limits.unwrap_or_else(|| panic!("坏字段应当退回缺省: {bad}"));

            assert_eq!(limits.frames_per_second, frames, "{bad}");
            assert_eq!(limits.chunks_per_second, chunks, "{bad}");
            assert_eq!(limits.bytes_per_second, bytes, "{bad}");
        }

        // 整个字段缺失 / 为 null / 根本不是对象，才是「没广告」
        for missing in [
            r#"{"type":"server.welcome","protocol":1,"peerOnline":true}"#,
            r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":null}"#,
            r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":"nope"}"#,
        ] {
            let ServerFrame::Welcome { limits, .. } =
                serde_json::from_str::<ServerFrame>(missing).unwrap()
            else {
                panic!("应当仍然是 welcome: {missing}");
            };

            assert!(limits.is_none(), "应当按「没广告」处理: {missing}");
        }

        // 广告值有上限：出错或恶意的中继不能把客户端速率推到任意高
        let ServerFrame::Welcome {
            limits: Some(capped),
            ..
        } = serde_json::from_str(
            r#"{"type":"server.welcome","protocol":1,"peerOnline":true,"limits":{"framesPerSecond":1e9,"chunksPerSecond":1e9,"bytesPerSecond":1e12}}"#,
        )
        .unwrap()
        else {
            panic!("应当解析出 limits");
        };

        assert_eq!(capped.frames_per_second, MAX_ADVERTISED_FRAMES_PER_SECOND);
        assert_eq!(capped.chunks_per_second, MAX_ADVERTISED_CHUNKS_PER_SECOND);
        assert_eq!(capped.bytes_per_second, MAX_ADVERTISED_BYTES_PER_SECOND);
    }

    /// R20：客户端不 1:1 取用广告值，按今天的比例留余量
    #[test]
    fn outbound_limits_keep_a_margin_below_the_advertised_values() {
        let cloudflare = RelayLimits::cloudflare().outbound();

        // 缺省推导必须与改造前完全一致：帧 20/20、分片 15/10
        assert_eq!(cloudflare.frames_per_second, 20.0);
        assert_eq!(cloudflare.frames_burst, 20.0);
        assert_eq!(cloudflare.chunks_per_second, 15.0);
        assert_eq!(cloudflare.chunks_burst, 10.0);

        // 自建中继广告 90 帧/秒时，客户端上限正好是 60（Phase 9a 的 60Hz 目标）
        let raised = RelayLimits {
            frames_per_second: 90.0,
            ..RelayLimits::cloudflare()
        }
        .outbound();

        assert_eq!(raised.frames_per_second, 60.0);
        assert!(raised.frames_per_second < 90.0, "必须给中继留出余量");

        // 离谱的小值不能推成 0（否则等待时长会除零）
        let tiny = RelayLimits {
            frames_per_second: 1.0,
            chunks_per_second: 1.0,
            bytes_per_second: 1.0,
        }
        .outbound();

        assert!(tiny.frames_per_second >= 1.0);
        assert!(tiny.chunks_per_second >= 1.0);
        assert!(tiny.chunks_burst >= 1.0);
    }

    #[test]
    fn parses_server_control_frames() {
        let welcome: ServerFrame =
            serde_json::from_str(r#"{"type":"server.welcome","protocol":1,"peerOnline":true}"#)
                .unwrap();
        let peer: ServerFrame =
            serde_json::from_str(r#"{"type":"server.peer","online":false,"deviceId":"abc"}"#)
                .unwrap();

        match welcome {
            ServerFrame::Welcome {
                protocol,
                peer_online,
                limits,
                ..
            } => {
                assert_eq!(protocol, 1);
                assert!(peer_online);
                // 旧中继不带 limits
                assert!(limits.is_none());
            }
            _ => panic!("expected welcome"),
        }

        match peer {
            ServerFrame::Peer { online, device_id } => {
                assert!(!online);
                assert_eq!(device_id, "abc");
            }
            _ => panic!("expected peer frame"),
        }
    }

    #[test]
    fn pet_snapshot_sends_key_names_but_never_pixels() {
        let snapshot = PetSnapshot {
            keyboard: PetKeyboardState {
                active: true,
                left_hand: true,
                right_hand: false,
                intensity: 0.6,
                keys: vec!["KeyA".to_string(), "ShiftLeft".to_string()],
            },
            pointer: PetPointerState {
                active: true,
                x: 0.34,
                y: 0.66,
                speed: 0.15,
                left_down: true,
                right_down: false,
            },
        };

        let json = serde_json::to_string(&snapshot).unwrap();

        // R37：键名是要发出去的（用户明确要求），但仍然没有像素坐标
        assert!(json.contains(r#""leftHand":true"#));
        assert!(json.contains(r#"["KeyA","ShiftLeft"]"#));
        assert!(!json.contains("KeyboardPress"));
        // 真实坐标不会出现：0.34 / 0.66 是比例，1920 / 1080 这种屏幕尺寸不能出现
        assert!(!json.contains("1920"));
        assert!(!json.contains("1080"));

        for number in [
            snapshot.keyboard.intensity,
            snapshot.pointer.x,
            snapshot.pointer.y,
        ] {
            assert!((0.0..=1.0).contains(&number));
        }
    }

    #[test]
    fn pet_snapshot_sanitizes_and_quantizes() {
        let messy = PetSnapshot {
            keyboard: PetKeyboardState {
                active: true,
                left_hand: true,
                right_hand: true,
                intensity: f32::NAN,
                keys: vec![
                    "KeyA".to_string(),
                    "KeyA".to_string(),
                    "Key B".to_string(),
                    "a".repeat(KEY_NAME_MAX + 1),
                    "KeyZ".to_string(),
                ],
            },
            pointer: PetPointerState {
                active: true,
                x: 1.5,
                y: -3.0,
                speed: 0.53,
                left_down: true,
                right_down: true,
            },
        };

        let clean = messy.sanitized();

        assert_eq!(clean.keyboard.intensity, 0.0);
        assert_eq!(clean.pointer.x, 1.0);
        assert_eq!(clean.pointer.y, 0.0);
        // 0.53 量化到 0.05 的整数倍
        assert_eq!(clean.pointer.speed, 0.55);
        assert!(clean.keyboard.active && clean.pointer.left_down);
        // R37：重复的、带空格的、超长的键名都被丢掉，剩下的排序
        assert_eq!(clean.keyboard.keys, vec!["KeyA".to_string(), "KeyZ".to_string()]);
    }

    #[test]
    fn pet_snapshot_caps_the_key_list() {
        let mut keys: Vec<String> = (0..20).map(|index| format!("Key{index}")).collect();

        let clean = sanitize_keys(std::mem::take(&mut keys));

        assert_eq!(clean.len(), KEY_LIST_MAX);
        // 排序之后再截断：留下的是字典序最小的那 8 个（`Key10` 排在 `Key2` 前面）
        assert_eq!(clean.first().map(String::as_str), Some("Key0"));
        assert_eq!(clean.last().map(String::as_str), Some("Key15"));

        // 畸形载荷一律丢掉，不 panic、不留下空壳
        assert!(sanitize_keys(vec!["".to_string(), "  ".to_string(), "键A".to_string()]).is_empty());
    }

    #[test]
    fn pet_snapshot_round_trip_through_envelope() {
        let snapshot = PetSnapshot::default();
        let envelope = AppEnvelope::new(
            message_type::PET_STATE,
            3,
            serde_json::to_value(&snapshot).unwrap(),
        );

        let parsed = AppEnvelope::from_bytes(&envelope.to_bytes().unwrap()).unwrap();
        let decoded: PetSnapshot = serde_json::from_value(parsed.payload).unwrap();

        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn input_stats_uses_camel_case_on_the_wire() {
        let stats = InputStats {
            date: "2026-09-23".into(),
            today_keyboard: 12,
            today_mouse: 3,
            total_keyboard: 4567,
            total_mouse: 89,
            share: true,
        };

        let json = serde_json::to_string(&stats).unwrap();

        assert!(json.contains(r#""todayKeyboard":12"#));
        assert!(json.contains(r#""totalMouse":89"#));
        assert!(json.contains(r#""share":true"#));

        let decoded: InputStats = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, stats);
    }

    /// 附件 offer 的线上格式（§39）：字段名走 camelCase，且只有文件名、没有本地路径
    #[test]
    fn transfer_offer_stays_camel_case_and_path_free() {
        let offer = TransferOfferPayload {
            transfer_id: 42,
            message_id: "m-1".into(),
            attachment_id: "a-1".into(),
            kind: TransferKind::File,
            name: "secret.zip".into(),
            size: 1234,
            mime: "application/zip".into(),
            sha256: "abc".into(),
            chunk_size: 524_288,
            chunks: 1,
        };

        let json = serde_json::to_string(&offer).unwrap();

        assert!(json.contains(r#""transferId":42"#));
        assert!(json.contains(r#""messageId":"m-1""#));
        assert!(json.contains(r#""kind":"file""#));
        assert!(!json.contains("C:\\"));

        let decoded: TransferOfferPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.chunk_size, 524_288);
        assert_eq!(decoded.kind, TransferKind::File);
    }

    #[test]
    fn transfer_kind_parses_known_values_only() {
        assert_eq!(TransferKind::parse("image"), Ok(TransferKind::Image));
        assert_eq!(TransferKind::parse("voice"), Ok(TransferKind::Voice));
        assert!(TransferKind::parse("movie").is_err());
        assert_eq!(TransferKind::File.as_str(), "file");
    }

    /// R21：信令的线上形状。`kind` 是 tag，字段名是 camelCase——两侧都是同一份代码，
    /// 但字段名写错只会在真机上暴露，所以钉在这里。
    #[test]
    fn signal_payloads_round_trip_with_the_wire_shape() {
        let hello = PairSignalPayload::Hello {
            version: SIGNAL_VERSION,
            device_id: "cat-a".to_string(),
            features: vec![FEATURE_RELIABLE_CHANNEL.to_string()],
        };

        let json = serde_json::to_value(&hello).unwrap();

        assert_eq!(json["kind"], "hello");
        assert_eq!(json["version"].as_u64(), Some(SIGNAL_VERSION.into()));
        assert_eq!(json["deviceId"], "cat-a");
        assert_eq!(json["features"][0], FEATURE_RELIABLE_CHANNEL);
        assert_eq!(
            serde_json::from_value::<PairSignalPayload>(json).unwrap(),
            hello
        );

        // R32：老客户端的 hello 没有 `features` 字段，必须照样能解出来（空能力 = 只支持
        // `pet-state` 通道）；反过来，空能力也不该往线上写一个空数组。
        let old =
            serde_json::json!({ "kind": "hello", "version": SIGNAL_VERSION, "deviceId": "cat-b" });

        assert_eq!(
            serde_json::from_value::<PairSignalPayload>(old).unwrap(),
            PairSignalPayload::Hello {
                version: SIGNAL_VERSION,
                device_id: "cat-b".to_string(),
                features: Vec::new(),
            }
        );
        assert!(
            serde_json::to_value(PairSignalPayload::Hello {
                version: SIGNAL_VERSION,
                device_id: "cat-b".to_string(),
                features: Vec::new(),
            })
            .unwrap()
            .get("features")
            .is_none()
        );

        let candidate = PairSignalPayload::Candidate {
            candidate: "candidate:1 1 udp".to_string(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
        };

        let json = serde_json::to_value(&candidate).unwrap();

        assert_eq!(json["kind"], "candidate");
        assert_eq!(json["sdpMid"], "0");
        assert_eq!(json["sdpMLineIndex"].as_u64(), Some(0));
        assert_eq!(
            serde_json::from_value::<PairSignalPayload>(json).unwrap(),
            candidate
        );

        // 不带 sdpMid / sdpMLineIndex 的候选也要能解（两端版本可能不同）
        assert_eq!(
            serde_json::from_value::<PairSignalPayload>(
                serde_json::json!({ "kind": "candidate", "candidate": "candidate:2 1 udp" })
            )
            .unwrap(),
            PairSignalPayload::Candidate {
                candidate: "candidate:2 1 udp".to_string(),
                sdp_mid: None,
                sdp_mline_index: None,
            }
        );

        // 未知 kind 直接拒绝：版本漂移时宁可什么都不做，也不要半懂不懂地打洞
        assert!(
            serde_json::from_value::<PairSignalPayload>(serde_json::json!({ "kind": "bye" }))
                .is_err()
        );
    }

    /// R21：`server.welcome` 的 `iceServers` 是可选字段，而且中继是对方维护的——畸形
    /// 输入只能让这一项退回空，不能让整帧解析失败（那会连 `peerOnline` 一起丢掉）。
    #[test]
    fn welcome_ice_servers_are_parsed_leniently() {
        let frame: ServerFrame = serde_json::from_str(
            r#"{
                "type": "server.welcome",
                "protocol": 1,
                "peerOnline": true,
                "iceServers": [
                    { "urls": ["stun:example.test:3478", "turn:example.test:3478"],
                      "username": "u", "credential": "c" },
                    { "urls": "stun:other.test:3478" }
                ]
            }"#,
        )
        .unwrap();

        let ServerFrame::Welcome { ice_servers, .. } = frame else {
            panic!("应当解析成 server.welcome");
        };

        assert_eq!(ice_servers.len(), 2);
        assert_eq!(
            ice_servers[0].urls,
            vec![
                "stun:example.test:3478".to_string(),
                "turn:example.test:3478".to_string()
            ]
        );
        assert_eq!(ice_servers[0].username, "u");
        assert_eq!(ice_servers[0].credential, "c");
        // `urls` 是单个字符串时也要收
        assert_eq!(
            ice_servers[1].urls,
            vec!["stun:other.test:3478".to_string()]
        );
        assert_eq!(ice_servers[1].username, "");

        for payload in [
            // 缺字段
            r#"{ "type": "server.welcome", "protocol": 1, "peerOnline": false }"#,
            // null
            r#"{ "type": "server.welcome", "protocol": 1, "peerOnline": false, "iceServers": null }"#,
            // 形状不对
            r#"{ "type": "server.welcome", "protocol": 1, "peerOnline": false, "iceServers": "nope" }"#,
            // 数组里坏掉的那条要单独丢掉，好的那条留下
            r#"{ "type": "server.welcome", "protocol": 1, "peerOnline": false,
                 "iceServers": [{ "urls": 3 }, { "urls": "stun:ok.test:3478" }] }"#,
        ] {
            let frame: ServerFrame = serde_json::from_str(payload).unwrap();

            let ServerFrame::Welcome { ice_servers, .. } = frame else {
                panic!("应当解析成 server.welcome");
            };

            assert!(ice_servers.len() <= 1, "畸形条目要丢掉：{ice_servers:?}");
        }
    }
}

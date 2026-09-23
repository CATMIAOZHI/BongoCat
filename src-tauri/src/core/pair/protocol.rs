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
    pub const PRESENCE: &str = "pair.presence";
    pub const PET_STATE: &str = "pair.pet-state";
    pub const STATS: &str = "pair.stats";
    pub const CHAT_TEXT: &str = "chat.text";
    pub const CHAT_ACK: &str = "chat.ack";
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

/// 键盘活动：只有「哪只手 + 强度」，永远不含具体键名（见 docs/pair-plan.md 的 §17 / §18 与 R2 / R3）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetKeyboardState {
    pub active: bool,
    pub left_hand: bool,
    pub right_hand: bool,
    /// 0..1，发送前量化到 0.2
    pub intensity: f32,
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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

/// 中继发来的明文控制帧
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    #[serde(rename = "server.welcome")]
    Welcome {
        protocol: u8,
        #[serde(rename = "peerOnline")]
        peer_online: bool,
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
            } => {
                assert_eq!(protocol, 1);
                assert!(peer_online);
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
    fn pet_snapshot_hides_key_names_and_pixels() {
        let snapshot = PetSnapshot {
            keyboard: PetKeyboardState {
                active: true,
                left_hand: true,
                right_hand: false,
                intensity: 0.6,
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

        // 只能出现布尔与 0..1 的比例，没有任何键名或像素坐标
        assert!(json.contains(r#""leftHand":true"#));
        assert!(!json.contains("KeyA"));
        assert!(!json.contains("KeyboardPress"));

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
    }

    #[test]
    fn pet_snapshot_round_trip_through_envelope() {
        let snapshot = PetSnapshot::default();
        let envelope = AppEnvelope::new(
            message_type::PET_STATE,
            3,
            serde_json::to_value(snapshot).unwrap(),
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
}

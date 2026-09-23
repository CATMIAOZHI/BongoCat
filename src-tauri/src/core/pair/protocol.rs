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
}

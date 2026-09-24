//! 中继协议常量、线上控制帧与校验规则。
//!
//! 这份文件是 `server-cloudflare/src/protocol.ts` 的 Rust 对等物：常量、关闭码与
//! 控制帧**逐条对齐**（要求的是行为一致，不要求代码相同；自建版只在
//! `server.welcome` 里多出可选的 `limits` / `iceServers` 字段，旧客户端会忽略未知
//! 字段）。任何一侧漂移，都会让客户端在已经部署的另一侧上失败（401 / 426 / 1008 这类）。

use serde::Serialize;

pub const PROTOCOL_VERSION: u8 = 1;

/// `GET /health`
pub const HEALTH_PATH: &str = "/health";

/// `GET /ws`：WebSocket 升级端点
pub const WS_PATH: &str = "/ws";

/// 唯一支持的 WebSocket 协议版本（RFC 6455）
pub const WEBSOCKET_VERSION: &str = "13";

pub const HEADER_AUTHORIZATION: &str = "authorization";
pub const HEADER_CLIENT: &str = "x-bongo-client";
pub const HEADER_PROTOCOL: &str = "x-bongo-protocol";
/// 多会话分组（§4）：客户端从 Pair Secret 派生出的 `ROOM_ID`。
///
/// 它只是一个 HTTP 升级头，不改帧格式、不改 `AppEnvelope`、不改协议版本——所以
/// 带这个头的新客户端仍然能连**旧** Cloudflare 中继（那边直接忽略它，§5 / §17）。
pub const HEADER_ROOM: &str = "x-bongo-room";

/// 每个应用帧固定 14 字节明文帧头：kind(1) | flags(1) | transferId(8) | seq(4)
pub const FRAME_HEADER_SIZE: usize = 14;

pub const FRAME_KIND_TRANSFER_CHUNK: u8 = 6;
pub const MAX_FRAME_KIND: u8 = 8;

/// 单帧上限（整帧，含帧头与 nonce/tag）
pub const MAX_BINARY_FRAME_SIZE: usize = 1024 * 1024;

/// 一个 Room（一个联机密钥）永远只有两台设备
pub const PAIR_SIZE: usize = 2;

/// 一套服务器同时承载的双人会话数上限（§2）。超出的**新会话**会被拒（HTTP 503），
/// 已经在跑的会话不受影响。
pub const DEFAULT_MAX_SESSIONS: usize = 20;

/// `ROOM_ID` 的长度上界。客户端派生出来的是 43 个字符（32 字节 base64url 无填充），
/// 这里按上界校验：中继只需要「非空、够短、字符集合法」，不必钉死长度。
pub const MAX_ROOM_ID_LENGTH: usize = 64;

/// 限流缺省值：容量就是这三个「每秒上限」，按时间连续补充（令牌桶）。
///
/// 缺省值与 CF 版一致；自建中继可以用环境变量调高（见 `main.rs`），并通过
/// `server.welcome` 的 `limits` 告诉客户端。
pub const DEFAULT_MAX_FRAMES_PER_SECOND: f64 = 30.0;
pub const DEFAULT_MAX_CHUNKS_PER_SECOND: f64 = 20.0;
/// 12 MiB：20 个 512 KiB chunk（每个含帧头与 nonce/tag 约 524 KiB）合计约 10 MiB
pub const DEFAULT_MAX_BYTES_PER_SECOND: f64 = 12.0 * 1024.0 * 1024.0;

/// 超过这个时间没有任何消息的连接可以被新连接顶替（2 倍心跳）
pub const DEFAULT_STALE_AFTER_MS: u64 = 120_000;

/// 最后活动时间最多每 10 秒写一次，避免高频写
pub const LAST_SEEN_WRITE_INTERVAL_MS: u64 = 10_000;

pub const MAX_DEVICE_ID_LENGTH: usize = 64;

pub mod close_code {
    /// 同一 deviceId 重连：旧连接被顶替
    pub const REPLACED: u16 = 4002;
    /// 第三个不同的客户端
    pub const PAIR_FULL: u16 = 4003;
    /// 顶替长时间无活动的连接
    pub const STALE: u16 = 4004;
    /// 协议 / 帧格式错误
    pub const PROTOCOL_ERROR: u16 = 1008;
    /// 帧过大
    pub const TOO_LARGE: u16 = 1009;
    /// 服务端内部错误
    pub const INTERNAL_ERROR: u16 = 1011;
}

pub fn is_known_frame_kind(kind: u8) -> bool {
    (1..=MAX_FRAME_KIND).contains(&kind)
}

/// deviceId 规则与 CF 版一致：非空、≤ 64 字符、只允许 `[A-Za-z0-9-]`。
/// 中继在比较前先归一成小写（同一个 UUID 用大写重连不能被当成第三个人）。
pub fn is_valid_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DEVICE_ID_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `ROOM_ID` 规则：非空、≤ 64 字符、只允许 `[A-Za-z0-9_-]`（base64url 字符集）。
///
/// 只做格式校验，不做长度钉死：中继不认识 Room，也不该认识——它只把这个值当分组键。
/// 大小写**敏感**（base64url 区分大小写），所以这里不做归一化。
pub fn is_valid_room_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ROOM_ID_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// 令牌桶额度。作为 `server.welcome` 的 `limits` 下发给客户端。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub frames_per_second: f64,
    pub chunks_per_second: f64,
    pub bytes_per_second: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
            chunks_per_second: DEFAULT_MAX_CHUNKS_PER_SECOND,
            bytes_per_second: DEFAULT_MAX_BYTES_PER_SECOND,
        }
    }
}

/// 服务端控制帧（明文 JSON，不含任何用户内容）。
///
/// `limits` / `iceServers` 是自建版新增的**可选**字段：旧客户端忽略未知字段，
/// 因此加它们不会破坏已部署的客户端。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    #[serde(rename = "server.welcome")]
    Welcome {
        protocol: u8,
        #[serde(rename = "peerOnline")]
        peer_online: bool,
        limits: Limits,
        #[serde(rename = "iceServers", skip_serializing_if = "Option::is_none")]
        ice_servers: Option<serde_json::Value>,
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

impl ServerFrame {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("控制帧永远可以序列化")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_keeps_the_cloudflare_shape_and_adds_optional_fields() {
        let frame = ServerFrame::Welcome {
            protocol: PROTOCOL_VERSION,
            peer_online: false,
            limits: Limits::default(),
            ice_servers: None,
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["type"], "server.welcome");
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["peerOnline"], false);
        assert_eq!(json["limits"]["framesPerSecond"], 30.0);
        assert_eq!(json["limits"]["chunksPerSecond"], 20.0);
        assert_eq!(json["limits"]["bytesPerSecond"], 12.0 * 1024.0 * 1024.0);
        // 没配 TURN 时整个字段都不出现
        assert!(json.get("iceServers").is_none());
    }

    #[test]
    fn welcome_carries_ice_servers_verbatim() {
        let servers = serde_json::json!([{ "urls": ["stun:cat.example.com:3478"] }]);
        let frame = ServerFrame::Welcome {
            protocol: PROTOCOL_VERSION,
            peer_online: true,
            limits: Limits::default(),
            ice_servers: Some(servers.clone()),
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["iceServers"], servers);
    }

    #[test]
    fn peer_frame_matches_the_cloudflare_shape() {
        let frame = ServerFrame::Peer {
            online: true,
            device_id: "0f8fad5b".to_string(),
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["type"], "server.peer");
        assert_eq!(json["online"], true);
        assert_eq!(json["deviceId"], "0f8fad5b");
    }

    #[test]
    fn frame_kinds_and_device_ids_are_validated_like_the_cloudflare_relay() {
        for kind in 1..=MAX_FRAME_KIND {
            assert!(is_known_frame_kind(kind));
        }
        assert!(!is_known_frame_kind(0));
        assert!(!is_known_frame_kind(MAX_FRAME_KIND + 1));

        assert!(is_valid_device_id("0F8FAD5B-A2C3-4E1F-9A0B-1C2D3E4F5A6B"));
        assert!(is_valid_device_id("-"));
        assert!(!is_valid_device_id(""));
        assert!(!is_valid_device_id("has space"));
        assert!(!is_valid_device_id("下划线_"));
        assert!(!is_valid_device_id(&"a".repeat(MAX_DEVICE_ID_LENGTH + 1)));
        assert!(is_valid_device_id(&"a".repeat(MAX_DEVICE_ID_LENGTH)));
    }
}

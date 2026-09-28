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

/// 服务器密码派生出来的凭据（R36）。它是**服务器级**的门槛，与 Room 无关：
/// 一个能连上你的人也必须知道部署者在服务器上设的密码，否则连一次握手都拿不到。
///
/// 它同样只是一个 HTTP 升级头：旧客户端不发它（会被这一版中继拒），带它的新客户端
/// 连旧自建中继与 Cloudflare 版都照旧可用（那边忽略未知头）。
pub const HEADER_SERVER: &str = "x-bongo-server";

/// 「我认得档位」这个能力标记（公益档用）。
///
/// 客户端**永远**带上它（值固定 `1`）：它自己也不知道用户填的是部署者密码还是公益密码
/// ——同一个输入框。部署者那一档完全无视这个头，Cloudflare 版与旧自建版忽略未知头。
///
/// 它换来的是一条**明确的兼容边界**：老客户端拿着公益密码会在握手时拿到 426（「你这版
/// 客户端还不认公益档，请升级」），而不是「连上之后被踢」或者「界面显示已连接、其实
/// 什么都通不了」。
pub const HEADER_TIER: &str = "x-bongo-tier";

/// 认得档位的客户端发过来的取值
pub const TIER_HEADER_VALUE: &str = "1";

/// 服务器密码的最小长度（部署者在 `.env` 里设置）。
///
/// 太短的密码会让「门槛」变成摆设：它保护的是「别人能不能白用你的服务器与 TURN」，
/// 而服务器没有任何其它限速手段。16 个字符已经远超在线爆破的可行范围（每次尝试都
/// 要先建一条 TCP + 发一次握手）。
pub const MIN_SERVER_PASSWORD_LENGTH: usize = 16;

/// 每个应用帧固定 14 字节明文帧头：kind(1) | flags(1) | transferId(8) | seq(4)
pub const FRAME_HEADER_SIZE: usize = 14;

pub const FRAME_KIND_TRANSFER_CHUNK: u8 = 6;
pub const MAX_FRAME_KIND: u8 = 8;

/// 公益档唯一放行的 kind：`pair.signal`（打洞信令）与 `pair.ping/pong`（保活）都走它。
///
/// 这一档是**策略与额度边界，不是密码学边界**：中继只读 14 字节明文帧头，`kind` 之内
/// 的一切（连载荷里的 `type` 字符串）都是 AEAD 密文，所以它无法区分「真信令」与「塞在
/// kind 8 里的任意数据」。挡住的量由公益档自己的额度决定（见下面那几个缺省值），而
/// TURN 凭据一个都不广告——那比带宽贵得多。
pub const FRAME_KIND_SIGNAL: u8 = 8;

/// 公益档的单帧上限（64 KiB）。
///
/// 打洞信令的报价含候选，量级是几 KB；给到 64 KiB 是留足余量，同时把「拿 kind 8 当
/// 夹带通道」压成涓流。超过就 `1009` 关连接。
pub const MAX_PUBLIC_FRAME_SIZE: usize = 64 * 1024;

/// 单帧上限（整帧，含帧头与 nonce/tag）
pub const MAX_BINARY_FRAME_SIZE: usize = 1024 * 1024;

/// 一个 Room（一个配对密码）永远只有两台设备
pub const PAIR_SIZE: usize = 2;

/// 一套服务器同时承载的双人会话数上限（§2）。超出的**新会话**会被拒（HTTP 503），
/// 已经在跑的会话不受影响。
pub const DEFAULT_MAX_SESSIONS: usize = 20;

/// 公益档同时承载的会话数上限（`PAIR_MAX_PUBLIC_SESSIONS`）。
///
/// 与 `PAIR_MAX_SESSIONS` **完全分开**：公益档占不到部署者自己的名额，部署者那一档也
/// 不会因为公益档满了而受影响。公益连接只放行小帧、几乎没有出站积压，所以一条连接的
/// 实际开销远小于 44 MiB 那个最坏值，10 组在 1GB 机器上是安全的。
pub const DEFAULT_MAX_PUBLIC_SESSIONS: usize = 10;

/// 同一个 IP 最多同时开几条**公益**连接（`PAIR_MAX_PUBLIC_PER_IP`）。
///
/// 只挡**新建会话**，同一会话的第二个人照旧进得来（不然同一个 NAT 下面的一对人会被自己
/// 挡住）。一条公益会话是两条连接，所以默认 4 = 两对。
pub const DEFAULT_MAX_PUBLIC_PER_IP: usize = 4;

/// 公益档的额度（`PAIR_PUBLIC_MAX_FRAMES_PER_SECOND` / `PAIR_PUBLIC_MAX_BYTES_PER_SECOND`）。
///
/// 信令一轮只有个位数帧、总共几 KB，10 帧/秒与 256 KiB/秒 都留了很大余量；它们的作用是
/// 把「拿 kind 8 夹带数据」限制成涓流，同时保护部署者的带宽与 CPU。
pub const DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND: f64 = 10.0;
pub const DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND: f64 = 256.0 * 1024.0;

/// 公益档的空闲回收窗口（`PAIR_PUBLIC_WINDOW_SECS`）。
///
/// **它是空闲回收器，不是「打洞截止时间」**：中继看不到 DataChannel 有没有建立成功
/// （信令是密文），所以任何「到点硬断」都会掐断**已经直连成功、正在正常使用**的会话
/// ——而中继一断，客户端是整条会话重启、直连也跟着重来。这里的判据是「多久没收到**任何**
/// 入站消息」，诚实客户端每 60 秒发一次 WebSocket Ping，180 秒 = 三次漏拍。
pub const DEFAULT_PUBLIC_WINDOW_SECS: u64 = 180;

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
    /// 公益档：空闲太久被回收（**不是**「打洞失败」，见 `DEFAULT_PUBLIC_WINDOW_SECS`）
    pub const PUBLIC_WINDOW: u16 = 4005;
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

/// 这次连接算哪一档（`server.welcome` 的 `tier`，也是「能不能转发数据」的判据）。
///
/// 档位**跟着连接走**，不跟着 Room 走：拿公益密码的人**永远**只是公益档，即使他碰巧和
/// 一个用部署者密码的人进了同一个会话（那说明两边填了不同的密码）。这条保证了
/// 「公益密码只能用来打洞」是一件与别人无关的性质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// 部署者那一档：打洞 + 中继兜底 + （配了才有的）TURN
    Full,
    /// 公益档：只转发信令、只广告 STUN、自己的名额与额度
    Public,
}

impl Tier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Public => "public",
        }
    }
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
        /// 这一档的档位（自建版独有；Cloudflare 版不发，客户端缺失时按 `full` 处理）。
        /// 老客户端不认识它——那是 serde 默认行为（忽略未知字段），所以加它不会让老
        /// 客户端崩或错乱。
        tier: Tier,
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
            tier: Tier::Full,
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["type"], "server.welcome");
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["peerOnline"], false);
        assert_eq!(json["limits"]["framesPerSecond"], 30.0);
        assert_eq!(json["limits"]["chunksPerSecond"], 20.0);
        assert_eq!(json["limits"]["bytesPerSecond"], 12.0 * 1024.0 * 1024.0);
        // 档位跟连接走：部署者那一档也要明说，客户端才能把「公益档」当成一个可判定的值
        assert_eq!(json["tier"], "full");
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
            tier: Tier::Full,
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["iceServers"], servers);
    }

    /// 公益档的档位名是线上契约的一部分：客户端按它决定「只准发信令」那一套限制
    #[test]
    fn the_public_tier_is_announced_lowercase() {
        let frame = ServerFrame::Welcome {
            protocol: PROTOCOL_VERSION,
            peer_online: false,
            limits: Limits {
                frames_per_second: DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND,
                chunks_per_second: DEFAULT_MAX_CHUNKS_PER_SECOND,
                bytes_per_second: DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND,
            },
            ice_servers: None,
            tier: Tier::Public,
        };
        let json: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();

        assert_eq!(json["tier"], "public");
        assert_eq!(json["limits"]["framesPerSecond"], 10.0);
        assert_eq!(json["limits"]["bytesPerSecond"], 256.0 * 1024.0);
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

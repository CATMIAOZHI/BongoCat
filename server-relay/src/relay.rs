//! 会话层：多个双人会话（Room）、令牌桶限流、Room 内 A ↔ B 转发、上下线控制帧。
//!
//! 行为逐条对齐 `server-cloudflare/src/pair.ts` 的 Durable Object：顶替顺序、
//! 「顶替之后还剩几个对端」的判定、`replaced` 过滤掉的伪离线通知，全部保持一样。
//!
//! 主要的差别是**分组**：CF 版一个部署就是一个 Room，这一版一套服务器同时承载
//! `PAIR_MAX_SESSIONS` 个（§2）。分组键是客户端给的 `ROOM_ID`，而**所有**跨连接
//! 的动作都必须先落进那个 Room：转发、上下线公告、同 deviceId 顶替、陈旧连接摘除、
//! 停摆对端剔除。Room 之间互不可见是这一层的硬不变量（§15）。另外两条与分组无关的
//! 差别（`4004` 离线帧的收件人集合、`FORWARD_TIMEOUT` 会摘掉停摆对端）逐条写在
//! `../README.md` 的「与 Cloudflare 版的差异」那一节。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::error::CapacityError;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::auth::{auth_verifier, constant_time_eq, room_fingerprint};
use crate::http::{write_upgrade, RequestHead};
use crate::protocol::{
    self, close_code, is_known_frame_kind, Limits, ServerFrame, Tier, FRAME_HEADER_SIZE,
    FRAME_KIND_SIGNAL, FRAME_KIND_TRANSFER_CHUNK, LAST_SEEN_WRITE_INTERVAL_MS,
    MAX_BINARY_FRAME_SIZE, MAX_PUBLIC_FRAME_SIZE, PAIR_SIZE,
};

/// 一条已经登记进某个 Room 的连接。
struct ClientEntry {
    id: u64,
    sender: mpsc::Sender<Message>,
    last_seen: Instant,
    /// 这条连接被移出所属 Room 时（顶替 / 停摆被摘）自动失效的信号，读循环据此退出。
    ///
    /// 只为了它的 `Drop` 存在：Room 里那份名单是「这条连接还算数」的唯一真相，一旦摘牌，
    /// 读循环必须跟着结束，否则 socket 会一直挂着——注册表说它离线，它却还在把
    /// 帧转给对方，两边都不会自愈。
    #[allow(dead_code)]
    ejected: oneshot::Sender<()>,
}

/// 一个双人会话：密钥相同的两个人落进同一个 Room，最多两台不同设备（§7 / §10）。
struct PairRoom {
    /// `SHA256(AUTH_TOKEN)`。中继**不保存明文 token**，后续连接按同样方式算一份，
    /// 与它做恒定时间比较（§4 / §9）。
    auth_hash: [u8; 32],
    /// 这个会话是**创建它的那条连接**的档位（决定它占哪一份名额）。
    ///
    /// 档位定在 Room 上而不是「每条连接各自算」，是为了让名额归属唯一：同一个会话的
    /// 两个人必须用同一类凭据——填错了会被 409 挡住，而不是让一个人有中继兜底、另一个人
    /// 什么都没有。
    tier: Tier,
    /// 创建这个会话时那条连接来自哪个 IP（IPv6 按 /64 归并）。公益档的每 IP 限额与它
    /// 挂钩，`sweep` 释放时靠它把账还回去。
    ip: IpKey,
    /// 按 deviceId 索引：同一个 deviceId 重连天然就是「换掉原来那条」。
    clients: HashMap<String, ClientEntry>,
    /// 已经由 `reserve` 放行、还没走进 `admit` 的连接数。
    ///
    /// 它让容量判定既不漏（握手途中也算占位）也不错放（握手失败要还回去）：
    /// `PAIR_MAX_SESSIONS` 限制的是同时存在的 Room 数，而 Room 从放行那一刻就已经
    /// 占住名额（§8）。
    pending: usize,
    created_at: Instant,
    last_active: Instant,
}

/// 每个连接的出站队列容量。
///
/// 队列是**有界**的，而且转发时用 `send().await` 等空位：对端 TCP 停摆（手机进
/// 隧道、笔记本睡眠）时，中继会停止读发送方，反压自然传回发送方的 socket，
/// 而不是在这里把内存吃光。真被拖死（见 `FORWARD_TIMEOUT`）才摘掉那个对端。
const OUTBOUND_QUEUE: usize = 32;

/// 转发时最多等一个对端消费多久；超时说明这条连接已经停摆，直接摘掉
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

/// 连接结束时最多等 writer 把队列里剩下的帧（例如那条关闭帧）发完多久。
///
/// 正常情况是毫秒级；只有对端 socket 停摆时才会等满——那时排队的帧本来就发不出去，
/// 直接中止 writer 让 socket 真正关闭。
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 令牌桶：容量 = 每秒上限，按经过的时间连续补充。
///
/// 用令牌桶而不是固定窗口，是因为固定窗口在边界会允许双倍突发，而 R4 要求
/// 「状态变化立即发送」，一次抖动就可能被误判成超限并关掉连接。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 这条连接所属档位的额度。公益档有自己的小额度（见 `RelayOptions::public_limits`），
    /// 所以桶自己记着该用哪一份，不必在每次扣额度时回头查档位。
    limits: Limits,
    frames: f64,
    chunks: f64,
    bytes: f64,
    updated_at: Instant,
}

impl Bucket {
    fn full(limits: Limits, now: Instant) -> Self {
        Self {
            limits,
            frames: limits.frames_per_second,
            chunks: limits.chunks_per_second,
            bytes: limits.bytes_per_second,
            updated_at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let limits = self.limits;
        let elapsed = now.duration_since(self.updated_at).as_secs_f64();

        self.updated_at = now;
        self.frames =
            (self.frames + elapsed * limits.frames_per_second).min(limits.frames_per_second);
        self.chunks =
            (self.chunks + elapsed * limits.chunks_per_second).min(limits.chunks_per_second);
        self.bytes = (self.bytes + elapsed * limits.bytes_per_second).min(limits.bytes_per_second);
    }

    /// 扣掉本次额度；不够就返回 false（连接随之关闭，所以负值不必回滚，与 CF 版一致）
    fn take(&mut self, now: Instant, frames: f64, chunks: f64, bytes: f64) -> bool {
        self.refill(now);

        self.frames -= frames;
        self.chunks -= chunks;
        self.bytes -= bytes;

        self.frames >= 0.0 && self.chunks >= 0.0 && self.bytes >= 0.0
    }
}

#[derive(Default)]
struct State {
    /// `ROOM_ID` → 双人会话
    rooms: HashMap<String, PairRoom>,
    buckets: HashMap<u64, Bucket>,
    /// 公益档的每 IP **会话**数（`IpKey` → 组数）。只算新建的那一组，房间空了（`sweep`）
    /// 就还回去；计数归零删键，别让扫描器留下小条目。
    public_rooms_per_ip: HashMap<IpKey, usize>,
}

/// `admit` 的结果
#[derive(Debug)]
enum Admit {
    Accepted {
        id: u64,
        peer_online: bool,
        ejected: oneshot::Receiver<()>,
    },
    /// 这个 Room 里已经有两台不同设备在线（同一个配对密码的第三台）
    Full,
}

/// `reserve` 的拒绝原因。
///
/// 两种都必须在 **WebSocket 升级之前**判定：客户端要按 HTTP 状态码区分「配对密码不对」
/// 与「服务器满员」，而升级成功之后只剩关闭码可以表达（§9 / §27）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomRejection {
    /// 同名的 Room 已经存在，但这次带来的 token 摘要对不上 → 401
    AuthMismatch,
    /// 同名的 Room 已经存在，但这次的档位与创建时不同（两边填了不同类型的密码）→ 409
    TierMismatch,
    /// 这是一个新 Room，而服务器已经承载了 `PAIR_MAX_SESSIONS` 个 → 503
    Capacity,
    /// 这个 IP 的公益会话已经开满（`PAIR_MAX_PUBLIC_PER_IP`）→ 429
    PublicIpLimit,
}

/// 一次已经被计进容量的入场许可：`reserve` 签发，`serve` 消费。
///
/// 它的意义是让「握手成功之前就占住名额」与「握手失败要还回去」都有明确的归属：
/// 拿到它就等于「这个 Room 的名额算在我头上」，所以**每一条签发都必须被消费或还回**，
/// 否则名额会永久泄漏。
#[derive(Debug)]
pub struct Reservation {
    room_id: String,
    auth_hash: [u8; 32],
    /// 这次连接算哪一档（welcome 要广告它，桶要按它取额度）
    tier: Tier,
    /// 这条连接来自哪个 IP（IPv6 按 /64 归并）。只在极少数「Room 被并发清掉、就地重建」
    /// 的路径上要用它，所以跟着许可一起走，不必再回头问调用方。
    ip: IpKey,
}

/// `Relay` 的全部构造参数。
///
/// 参数已经有十来项，全平铺进 `new()` 会让每个调用点都变成一串看不出含义的位置参数
/// （公益档又加了 6 项）；这里一次性收成一个结构，调用点只写关心的字段。
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// 部署者那一档的额度（也是它的桶容量）
    pub limits: Limits,
    /// 公益档自己的额度。公益档只放行小帧（信令），所以这份值比 `limits` 小得多
    pub public_limits: Limits,
    /// 部署者那一档能同时承载的会话数（`PAIR_MAX_SESSIONS`）
    pub max_sessions: usize,
    /// 公益档能同时承载的会话数（`PAIR_MAX_PUBLIC_SESSIONS`）。0 = 公益档关闭
    pub max_public_sessions: usize,
    /// 同一个 IP 最多几条公益连接（`PAIR_MAX_PUBLIC_PER_IP`）。0 = 不限
    pub max_public_per_ip: usize,
    /// 公益档的空闲回收窗口：多久没收到任何入站消息就断开。`None` = 不回收
    pub public_window: Option<Duration>,
    /// 多久没有消息的连接可以被新连接顶替
    pub stale_after: Duration,
    /// `server.welcome` 里附带的 ICE 服务器（`PAIR_ICE_SERVERS`，可选，原样透传）
    pub ice_servers: Option<serde_json::Value>,
    /// 内置 STUN 的 UDP 端口（`None` = 没有内置 STUN，见 `stun.rs`）。有它而 `ice_servers`
    /// 为空时，welcome 里广告 `stun:<客户端连进来用的主机名>:<端口>`。
    pub stun_port: Option<u16>,
    /// R36：`SHA256(derive_server_token(服务器密码))`。这一版中继**必须**有它：
    /// 它是「谁能连上这台服务器」的唯一门槛，缺了它任何人都能白用转发与 TURN。
    /// 与 Room 的 verifier 一样只存摘要——启动之后进程里没有密码原文。
    pub server_verifier: [u8; 32],
    /// 公益密码的 verifier（`PAIR_PUBLIC_SERVER_PASSWORD`）。`None` = 这台服务器没有公益档
    pub public_verifier: Option<[u8; 32]>,
    /// 信不信 `X-Forwarded-For`（`PAIR_TRUST_PROXY`）。只在前面有可信反代时打开
    pub trust_proxy: bool,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            public_limits: Limits {
                frames_per_second: protocol::DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND,
                chunks_per_second: protocol::DEFAULT_MAX_CHUNKS_PER_SECOND,
                bytes_per_second: protocol::DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND,
            },
            max_sessions: protocol::DEFAULT_MAX_SESSIONS,
            max_public_sessions: protocol::DEFAULT_MAX_PUBLIC_SESSIONS,
            max_public_per_ip: protocol::DEFAULT_MAX_PUBLIC_PER_IP,
            public_window: Some(Duration::from_secs(protocol::DEFAULT_PUBLIC_WINDOW_SECS)),
            stale_after: Duration::from_millis(protocol::DEFAULT_STALE_AFTER_MS),
            ice_servers: None,
            stun_port: None,
            server_verifier: [0u8; 32],
            public_verifier: None,
            trust_proxy: false,
        }
    }
}

pub struct Relay {
    options: RelayOptions,
    next_id: AtomicU64,
    state: Mutex<State>,
}

impl Relay {
    pub fn new(options: RelayOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        })
    }

    pub fn trust_proxy(&self) -> bool {
        self.options.trust_proxy
    }

    /// 这台服务器**实际上**有没有公益档（`/health`、启动输出与凭据判定共用同一个判据）。
    ///
    /// 光配了密码还不算：名额为 0 时那一档一个人都进不来（`max_sessions_for` 会让每个
    /// 公益连接拿到 `Capacity` → HTTP 503，客户端还会按退避一直重试）。所以「配了密码」
    /// 与「开着」是两件事，`/health` 必须报后者——部署者就是拿这个接口确认自己装对了
    /// 没有，报错方向的话他查的是一条不存在的故障。
    pub fn has_public_tier(&self) -> bool {
        self.options.public_verifier.is_some() && self.options.max_public_sessions > 0
    }

    /// 这一档能同时承载多少个会话。公益档的名额与部署者那一档**完全分开**：
    /// 公益档再热闹也占不到部署者自己的名额，反之亦然。
    fn max_sessions_for(&self, tier: Tier) -> usize {
        match tier {
            Tier::Full => self.options.max_sessions,
            Tier::Public => self.options.max_public_sessions,
        }
    }

    /// 这一档的额度（也是它每个连接的桶容量）
    fn limits_for(&self, tier: Tier) -> Limits {
        match tier {
            Tier::Full => self.options.limits,
            Tier::Public => self.options.public_limits,
        }
    }

    /// 这次连接带来的服务器凭据算哪一档（R36 + 公益档）。`None` = 凭据不对，拒绝。
    ///
    /// 恒定时间比较、与长度无关的旁路不成立（两边都是 32 字节摘要）。**两次比较都要跑完
    /// 再决定**：命中就提前返回会让「命中了哪一档」通过时间差漏出去。
    ///
    /// 公益档被关掉（名额 0）时**故意**不认那把钥匙：它就等于「这台服务器没有公益档」，
    /// 于是那些人拿到的是 403「服务器密码不正确」，而不是一条会让客户端无限退避重试的
    /// 503「会话已满」——后者说的是一件没发生的事（名额根本没被占满）。
    pub fn classify_server_token(&self, token: &str) -> Option<Tier> {
        let verifier = auth_verifier(token);
        let full = constant_time_eq(&verifier, &self.options.server_verifier);
        let public = self.has_public_tier()
            && self
                .options
                .public_verifier
                .is_some_and(|expected| constant_time_eq(&verifier, &expected));

        if full {
            Some(Tier::Full)
        } else if public {
            Some(Tier::Public)
        } else {
            None
        }
    }

    /// 这次连接的 welcome 里该广告哪些 ICE 服务器。
    ///
    /// 部署者配了 `PAIR_ICE_SERVERS` 就原样用它；否则有内置 STUN 时，用客户端连进来时的
    /// `Host` 拼出 `stun:<主机名>:<端口>`——客户端怎么找到这台服务器的，就怎么找到它的
    /// STUN，部署者什么都不用填。`Host` 缺失或形状不对时不广告（P2P 退回只有内网地址）。
    ///
    /// **公益档只拿 `stun:`**：`turn:` 的条目（连带按流量计费的凭据）一个都不给，而且
    /// 顺手把 `username` / `credential` 字段从留下的小条目里摘掉（纵深防御）。STUN 本身
    /// 就不做鉴权（谁问都答，见 `stun.rs`），所以给公益档不新增任何暴露；不给的话公益档
    /// 只剩内网地址，跨网络一定打不通，那一档等于没有。
    pub fn ice_servers_for(&self, host: Option<&str>, tier: Tier) -> Option<serde_json::Value> {
        if let Some(servers) = &self.options.ice_servers {
            return match tier {
                Tier::Full => Some(servers.clone()),
                Tier::Public => stun_only(servers),
            };
        }

        let port = self.options.stun_port?;
        let name = crate::stun::host_without_port(host?)?;

        Some(serde_json::json!([{ "urls": [format!("stun:{name}:{port}")] }]))
    }

    /// 升级之前的容量与密钥判定（§8 / §9 / §27）。放行时**当场创建 Room**，让这份名额
    /// 从这个连接算起；连接失败由 `release`（或 `serve` 内部的失败路径）还回来。
    pub async fn reserve(
        &self,
        room_id: &str,
        auth_hash: [u8; 32],
        tier: Tier,
        ip: IpKey,
    ) -> Result<Reservation, RoomRejection> {
        let mut state = self.state.lock().await;
        let now = Instant::now();

        match state.rooms.get_mut(room_id) {
            Some(room) => {
                if !constant_time_eq(&room.auth_hash, &auth_hash) {
                    return Err(RoomRejection::AuthMismatch);
                }

                // 档位定在 Room 上（见 `PairRoom::tier`）：两边必须填同一类密码。填错了
                // 这里挡住并给出指向，比「一个人有中继兜底、另一个人什么都没有」好。
                if room.tier != tier {
                    return Err(RoomRejection::TierMismatch);
                }

                room.pending += 1;
            }
            None => {
                // 满了只挡**新建**会话；已经在跑的 Room 走上面那一支，不受影响（§8）。
                // 两档各算各的名额：公益档再多也占不到部署者自己的名额（反之亦然）。
                let used = state
                    .rooms
                    .values()
                    .filter(|room| room.tier == tier)
                    .count();

                if used >= self.max_sessions_for(tier) {
                    return Err(RoomRejection::Capacity);
                }

                // 同一个 IP 最多开几组公益会话。只挡新建：同一对用户的第二个人照旧进得来
                // （不然同一个 NAT 下面的一对人会被自己挡住）。
                if tier == Tier::Public
                    && self.options.max_public_per_ip > 0
                    && state
                        .public_rooms_per_ip
                        .get(&ip)
                        .copied()
                        .unwrap_or_default()
                        >= self.options.max_public_per_ip
                {
                    return Err(RoomRejection::PublicIpLimit);
                }

                if tier == Tier::Public {
                    *state.public_rooms_per_ip.entry(ip).or_default() += 1;
                }

                state.rooms.insert(
                    room_id.to_string(),
                    PairRoom {
                        auth_hash,
                        tier,
                        ip,
                        clients: HashMap::new(),
                        pending: 1,
                        created_at: now,
                        last_active: now,
                    },
                );
            }
        }

        Ok(Reservation {
            room_id: room_id.to_string(),
            auth_hash,
            tier,
            ip,
        })
    }

    /// 还回一份没用掉的入场许可（握手失败、后面的参数校验没过）。
    pub async fn release(&self, reservation: Reservation) {
        let mut state = self.state.lock().await;

        release_pending(&mut state, &reservation.room_id);
    }

    /// 完成握手、登记连接、跑读循环，直到这条连接结束。
    ///
    /// `head` 是已经被读掉的请求头（见 `http.rs`），这里不再重新解析。
    /// `reservation` 由 `reserve` 签发，在这个函数里被消费——**成功走到 `admit`，
    /// 或者在失败路径上还回去**，两条路都必须走完一条。
    pub async fn serve(
        self: Arc<Self>,
        stream: TcpStream,
        head: RequestHead,
        device_id: String,
        reservation: Reservation,
    ) -> Result<(), String> {
        let room_id = reservation.room_id.clone();
        let tier = reservation.tier;

        let Some(key) = head.header("sec-websocket-key") else {
            self.release(reservation).await;

            return Err("缺少 Sec-WebSocket-Key".into());
        };

        let accept_key = derive_accept_key(key.trim().as_bytes());
        let mut stream = stream;

        if let Err(error) = write_upgrade(&mut stream, &accept_key).await {
            // 升级没成功就还回名额：否则一个连不上的客户端会永久占住一个会话位
            self.release(reservation).await;

            return Err(error.to_string());
        }

        // 协议上限是 1 MiB，但这里故意把 WebSocket 层放到 8 MiB：超过 1 MiB 的帧必须
        // 由我们自己读完、再用 `close 1009` 关掉（这是线上契约的一部分）。
        //
        // 为什么不能贴着 1 MiB 设：交给 tungstenite 的 `max_message_size` 时它会在读完
        // 帧头后立刻报错，帧体还留在接收缓冲里；这时关连接会让 TCP 直接 RST，对端拿到
        // 的是「连接被重置」而不是 1009。留出这段余量，1 MiB ~ 8 MiB 的帧就能被完整
        // 读完、干净地关掉；超过 8 MiB 才落到「尽力发 1009」的那条路径（见读循环）。
        // `WebSocketConfig` 是 non_exhaustive，只能先取默认值再改字段
        let mut config = WebSocketConfig::default();

        config.max_message_size = Some(MAX_BINARY_FRAME_SIZE * 8);
        config.max_frame_size = Some(MAX_BINARY_FRAME_SIZE * 8);
        // 写缓冲也设上限：默认是无限，碰到对端停摆时同样会吃内存（真正的流控靠
        // 上面那个有界队列，这里只是兜底）
        config.max_write_buffer_size = 4 * MAX_BINARY_FRAME_SIZE;

        let mut websocket =
            WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
        let (sender, receiver) = mpsc::channel::<Message>(OUTBOUND_QUEUE);

        // 先决定能不能进：满了就握手后立刻用 4003 关掉（CF 版同样是「升级成功再关」，
        // 客户端才能把 4003 显示成「该联机会话已有两台设备在线」而不是一次普通连接失败）。
        let (id, mut ejected) = match self.admit(reservation, &device_id, &sender).await {
            Admit::Full => {
                let _ = websocket
                    // reason 与 CF 版逐字一致（`pair is full`）：关闭码才是契约，文案不是，
                    // 但两个中继说同一句话能省掉一次「为什么这边说的是 room」的排查
                    .send(Message::Close(Some(close_frame(
                        close_code::PAIR_FULL,
                        "pair is full",
                    ))))
                    .await;
                let _ = websocket.close(None).await;

                return Ok(());
            }
            Admit::Accepted {
                id,
                peer_online,
                ejected,
            } => {
                let welcome = ServerFrame::Welcome {
                    protocol: protocol::PROTOCOL_VERSION,
                    peer_online,
                    limits: self.limits_for(tier),
                    ice_servers: self.ice_servers_for(head.header("host"), tier),
                    tier,
                }
                .to_json();

                // 此刻还没有 writer 任务，独占 socket，直接发
                if let Err(error) = websocket.send(Message::Text(welcome.into())).await {
                    // 已经登记进 Room 了，必须先摘牌：否则会留下一个「幽灵对端」
                    // ——占着配对位，还让对方一直以为它在线上。
                    self.drop_peer(&room_id, &device_id, id).await;

                    return Err(error.to_string());
                }

                (id, ejected)
            }
        };

        let (mut sink, mut stream_half) = websocket.split();
        let mut writer = tokio::spawn(async move {
            let mut receiver = receiver;

            while let Some(message) = receiver.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }

            let _ = sink.close().await;
        });

        // 公益档的空闲回收（见 `protocol.rs` 的 `DEFAULT_PUBLIC_WINDOW_SECS`）。
        //
        // 它是**空闲回收器**，不是「打洞截止时间」：中继看不到 DataChannel 有没有建立
        // 成功（信令是密文），所以任何「到点硬断」都会掐断已经直连成功、正在正常使用的
        // 会话——而中继一断，客户端是整条会话重启、直连也跟着重来。判据是「多久没收到
        // **任何**入站消息」，诚实客户端每 60 秒发一次 WebSocket Ping。
        let window = self.options.public_window.filter(|_| tier == Tier::Public);
        let mut idle_deadline = window.map(|window| tokio::time::Instant::now() + window);

        loop {
            // 把截止时刻拷出来（`Option<Instant>` 是 `Copy`）：`select!` 那一支要借它，
            // 另一支要可变借 `idle_deadline`，同一个变量同时借两次过不了借用检查
            let idle_at = idle_deadline;
            let idle = async {
                match idle_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };

            let item = tokio::select! {
                // 注册表里已经没有这条连接了：读循环必须跟着结束，否则 socket 会一直
                // 挂着（见 `Peer::ejected`）。
                _ = &mut ejected => break,
                _ = idle => {
                    let _ = sender.try_send(Message::Close(Some(close_frame(
                        close_code::PUBLIC_WINDOW,
                        "public window idle",
                    ))));

                    break;
                }
                item = stream_half.next() => item,
            };

            let Some(item) = item else { break };

            // 任何入站消息都续期——包括下面那条会被忽略的 WS Ping/Pong：那是诚实客户端
            // 的节拍，正是「这条连接还活着」的证据。
            if let (Some(window), Some(deadline)) = (window, idle_deadline.as_mut()) {
                *deadline = tokio::time::Instant::now() + window;
            }

            let message = match item {
                Ok(message) => message,
                Err(error) => {
                    // 超过 WebSocket 层上限（8 MiB）的帧也尽力用 1009 关闭，别无声断开。
                    // 这一档已经超出契约上限 8 倍，帧体还留在接收缓冲里，所以关连接时
                    // 对端可能只看到连接被重置——两种结果对客户端是同一件事（重连）。
                    if let Some(frame) = too_large_close(&error) {
                        let _ = sender.try_send(Message::Close(Some(frame)));
                    }

                    break;
                }
            };

            match message {
                Message::Binary(bytes) => {
                    if bytes.len() > MAX_BINARY_FRAME_SIZE {
                        // 队列满时发不出关闭帧也无所谓：连接马上会因为 socket 关闭被对端发现
                        let _ = sender.try_send(Message::Close(Some(close_frame(
                            close_code::TOO_LARGE,
                            "frame too large",
                        ))));

                        break;
                    }

                    if bytes.len() < FRAME_HEADER_SIZE {
                        let _ = sender.try_send(Message::Close(Some(close_frame(
                            close_code::PROTOCOL_ERROR,
                            "malformed frame",
                        ))));

                        break;
                    }

                    let kind = bytes[0];

                    if !is_known_frame_kind(kind) {
                        let _ = sender.try_send(Message::Close(Some(close_frame(
                            close_code::PROTOCOL_ERROR,
                            "unknown frame kind",
                        ))));

                        break;
                    }

                    // 公益档只放行信令（kind 8：`pair.signal` 与 `pair.ping/pong`）。
                    //
                    // 这是**策略与额度边界，不是密码学边界**：中继只读 14 字节明文帧头，
                    // 解不开载荷，所以它挡不住「把数据塞进 kind 8」，只能把量压成涓流
                    // （配合公益档自己的小额度与 64 KiB 单帧上限）。
                    //
                    // 这里选择**如实拒绝**而不是静默丢：静默丢会让界面显示「已连接」，
                    // 而对方猫不动、消息发不出去，会话还无限期占着一个公益名额。
                    if tier == Tier::Public {
                        if bytes.len() > MAX_PUBLIC_FRAME_SIZE {
                            let _ = sender.try_send(Message::Close(Some(close_frame(
                                close_code::TOO_LARGE,
                                "frame too large for the public tier",
                            ))));

                            break;
                        }

                        if kind != FRAME_KIND_SIGNAL {
                            println!(
                                "[{}] 公益档拒绝了数据帧（kind {kind}，device {device_id}）",
                                room_fingerprint(&room_id)
                            );

                            let _ = sender.try_send(Message::Close(Some(close_frame(
                                close_code::PROTOCOL_ERROR,
                                "public tier carries signaling only",
                            ))));

                            break;
                        }
                    }

                    let chunks = if kind == FRAME_KIND_TRANSFER_CHUNK {
                        1.0
                    } else {
                        0.0
                    };

                    if !self.allow(id, 1.0, chunks, bytes.len() as f64).await {
                        let _ = sender.try_send(Message::Close(Some(close_frame(
                            close_code::PROTOCOL_ERROR,
                            "rate limit exceeded",
                        ))));

                        break;
                    }

                    self.touch(&room_id, &device_id, id).await;
                    self.forward(&room_id, id, Message::Binary(bytes)).await;
                }
                Message::Text(_) => {
                    // text 帧只用于服务端 → 客户端的控制帧。客户端发 text 属于协议偏离：
                    // 若原样转发，已配对的一方能伪造 server.* 控制帧（例如谎报对方上下线），
                    // 而且这条路径不经过 AEAD。
                    let _ = sender.try_send(Message::Close(Some(close_frame(
                        close_code::PROTOCOL_ERROR,
                        "client text frames are not accepted",
                    ))));

                    break;
                }
                Message::Close(_) => break,
                // Ping 由 tungstenite 在读循环里自动回 Pong（`protocol/mod.rs` 的
                // `read` 会先 flush 排队的 pong）；这里再发一条会让对端收到两个 pong。
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }

        drop(sender);

        // 顺序很重要：注册表里也握着一份 sender，必须先摘掉它，writer 的 channel
        // 才会关闭；否则 `writer.await` 会一直等下去，离线通知永远发不出去。
        // 摘掉之后 channel 里已经排队的帧（例如刚才那条关闭帧）仍会被 writer 发完。
        self.drop_peer(&room_id, &device_id, id).await;

        // writer 有可能正卡在往一个停摆的 socket 写数据上：那时排队的关闭帧永远发不
        // 出去，`writer.await` 也永远不返回，socket 会一直挂着。给它一点时间把队列排
        // 空，超时就中止任务——中止会丢掉 sink，连接随之真正关闭。
        if tokio::time::timeout(WRITER_DRAIN_TIMEOUT, &mut writer)
            .await
            .is_err()
        {
            writer.abort();
            let _ = writer.await;
        }

        Ok(())
    }

    /// 决定新连接能不能进这个 Room，并处理顶替。返回值里的 `peer_online` 用于 welcome。
    ///
    /// `reservation` 在这里被消费：名额已经用掉了，接受与否都不再还回去——拒绝只可能
    /// 发生在「Room 里已经有两台不同设备在线」的时候，而那个 Room 本来就占着名额。
    async fn admit(
        &self,
        reservation: Reservation,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
    ) -> Admit {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let room_id = reservation.room_id.as_str();
        let tier = reservation.tier;
        let ip = reservation.ip;

        // 预留一定要在这里减掉：漏一次这个 Room 就永远清不掉（容量泄漏）。
        match state.rooms.get_mut(room_id) {
            Some(room) => room.pending = room.pending.saturating_sub(1),
            // 理论上到不了：`reserve` 刚刚创建或命中过这个 Room。真被并发的清理摘掉时
            // 就地按同一份 verifier 重建——名额在 `reserve` 那一侧已经算过，这里不重复判定。
            //
            // 但**每 IP 的账要跟着记上**：`sweep` 摘掉一个公益 Room 时会对那个 IP
            // `saturating_sub` 一次，重建时不补记的话这份账就比配置更松（减到 0 之后
            // 后面几组白送）。会话名额是按 Room 现数出来的，所以只有这一份独立的计数
            // 需要在这里对齐。
            None => {
                if tier == Tier::Public {
                    *state.public_rooms_per_ip.entry(ip).or_default() += 1;
                }

                state.rooms.insert(
                    room_id.to_string(),
                    PairRoom {
                        auth_hash: reservation.auth_hash,
                        tier,
                        ip,
                        clients: HashMap::new(),
                        pending: 0,
                        created_at: now,
                        last_active: now,
                    },
                );
            }
        }

        let (others, stale) = {
            let room = &state.rooms[room_id];

            let others: Vec<String> = room
                .clients
                .keys()
                .filter(|other| other.as_str() != device_id)
                .cloned()
                .collect();

            // 先算出「顶替之后还剩几个对端」，再决定是否动手关连接：先关再拒绝会把发起方
            // 自己原来的连接也关掉，让本来能恢复的情况变成完全连不上。
            let stale = if others.len() >= PAIR_SIZE {
                others
                    .iter()
                    .filter(|other| {
                        now.duration_since(room.clients[*other].last_seen)
                            > self.options.stale_after
                    })
                    // 顶替「先连进来的那一个」：CF 版按 Durable Object 的 WebSocket 插入
                    // 顺序找第一个陈旧的连接，而每条连接的 id 正是按到达顺序单调发下去的，
                    // 所以取最小的 id 与那边完全等价——`HashMap` 的迭代顺序是随机的，
                    // 直接用它会让「顶替谁」变成抽签。
                    .min_by_key(|other| room.clients[*other].id)
                    .cloned()
            } else {
                None
            };

            (others, stale)
        };

        let remaining = others
            .iter()
            .filter(|other| Some(*other) != stale.as_ref())
            .count();

        if remaining >= PAIR_SIZE {
            // 同一个配对密码的第三方无法进入：这是体验约束，不是安全边界（R9 / §10）
            return Admit::Full;
        }

        // 同一 deviceId 重连（例如切换网络）：关掉旧连接再接受新连接
        if let Some(entry) = take_client(&mut state, room_id, device_id) {
            let _ = entry.sender.try_send(Message::Close(Some(close_frame(
                close_code::REPLACED,
                "replaced by a newer connection",
            ))));
        }

        if let Some(stale_device_id) = stale {
            if let Some(entry) = take_client(&mut state, room_id, &stale_device_id) {
                let _ = entry.sender.try_send(Message::Close(Some(close_frame(
                    close_code::STALE,
                    "stale connection replaced",
                ))));

                // CF 版靠被顶替连接的 close 事件补这条离线通知（`announceOffline` 只过滤
                // 同一 deviceId 的伪通知，不排除 4004），所以这里也要补，否则同一个场景
                // 在两侧会发出不同的控制帧。放在新连接上线**之前**：存活方看到的是
                // 「旧的离线 → 新的上线」，终态仍然是在线（CF 靠事件时序拿到的是反过来的
                // 顺序，反而会以「离线」收尾）。
                announce_offline(&mut state, room_id, entry.id, stale_device_id);
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (ejected, ejected_receiver) = oneshot::channel();

        state.buckets.insert(id, Bucket::full(self.limits_for(tier), now));

        {
            let room = state
                .rooms
                .get_mut(room_id)
                .expect("`reserve` 刚刚确保过这个 Room 存在");

            if room.clients.is_empty() {
                println!("[{}] 双人会话建立", room_fingerprint(room_id));
            }

            room.clients.insert(
                device_id.to_string(),
                ClientEntry {
                    id,
                    sender: sender.clone(),
                    last_seen: now,
                    ejected,
                },
            );

            room.last_active = now;
        }

        broadcast(
            &mut state,
            room_id,
            id,
            ServerFrame::Peer {
                online: true,
                device_id: device_id.to_string(),
            },
        );

        Admit::Accepted {
            id,
            peer_online: remaining > 0,
            ejected: ejected_receiver,
        }
    }

    /// 收尾一条连接。
    ///
    /// 必须在**所属 Room** 里按 `(deviceId, connectionId)` 双重匹配（§14）：同一个
    /// deviceId 的新连接已经把旧连接顶掉时，旧连接那条迟到的清理绝不能把新连接删掉
    /// ——那是「新连接刚连上就被判离线」，两端都会卡住。
    async fn drop_peer(&self, room_id: &str, device_id: &str, id: u64) {
        let mut state = self.state.lock().await;

        let is_current = state
            .rooms
            .get(room_id)
            .and_then(|room| room.clients.get(device_id))
            .is_some_and(|entry| entry.id == id);

        // 名单里那条不是我了：这是一条已经被顶替的旧连接的迟到清理，什么都别做
        if !is_current {
            return;
        }

        let Some(entry) = take_client(&mut state, room_id, device_id) else {
            return;
        };

        announce_offline(&mut state, room_id, entry.id, device_id.to_string());
        sweep(&mut state, room_id);
    }

    /// 最后活动时间最多每 10 秒更新一次，避免高频写（顶替判定只需要粗粒度）
    async fn touch(&self, room_id: &str, device_id: &str, id: u64) {
        let mut state = self.state.lock().await;
        let now = Instant::now();

        let Some(room) = state.rooms.get_mut(room_id) else {
            return;
        };

        room.last_active = now;

        let Some(entry) = room.clients.get_mut(device_id) else {
            return;
        };

        // 只有当前这条连接能刷新自己的时钟：旧连接的帧不该让新连接看起来还活着
        if entry.id == id
            && now.duration_since(entry.last_seen)
                >= Duration::from_millis(LAST_SEEN_WRITE_INTERVAL_MS)
        {
            entry.last_seen = now;
        }
    }

    async fn allow(&self, id: u64, frames: f64, chunks: f64, bytes: f64) -> bool {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        // 已经不在名单里的连接不再有桶：`entry().or_insert_with()` 会把桶重建出来，
        // 被摘掉的对端再发一帧就永久留下一条残留。这里直接拒绝，让读循环退出。
        let Some(bucket) = state.buckets.get_mut(&id) else {
            return false;
        };

        bucket.take(now, frames, chunks, bytes)
    }

    /// 只转发给**同一个 Room** 的对端，不回发给发送者，也不遍历别的 Room（§15）。
    ///
    /// 队列是有限的：对端消费不过来时这里会 await 等空位（反压回发送方的 TCP），
    /// 而不是把帧堆在内存里。真被拖过 `FORWARD_TIMEOUT` 就把那个对端摘掉（只摘那一条，
    /// 而且只摘在它自己的 Room 里）——一条停摆的连接不该拖死整台中继。
    async fn forward(&self, room_id: &str, from: u64, message: Message) {
        let targets: Vec<(String, u64, mpsc::Sender<Message>)> = {
            let state = self.state.lock().await;

            match state.rooms.get(room_id) {
                None => Vec::new(),
                Some(room) => room
                    .clients
                    .iter()
                    .filter(|(_, entry)| entry.id != from)
                    .map(|(device_id, entry)| (device_id.clone(), entry.id, entry.sender.clone()))
                    .collect(),
            }
        };

        for (device_id, id, sender) in targets {
            match tokio::time::timeout(FORWARD_TIMEOUT, sender.send(message.clone())).await {
                Ok(Ok(())) => {}
                _ => {
                    // 队列已满且超时 / channel 已关闭：这个对端已经停摆
                    self.drop_peer(room_id, &device_id, id).await;
                }
            }
        }
    }
}

/// 一个 IP 的归并键。
///
/// IPv4 按 /32（就是一个地址）；**IPv6 按 /64** 归并：一台机器随手就能换出同一段 /64
/// 里的地址，只按 /32 记等于没限额。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpKey([u8; 16]);

impl IpKey {
    fn from_addr(address: IpAddr) -> Self {
        // 双栈 socket 上 IPv4 客户端会以 `::ffff:a.b.c.d` 出现，先归一：不归一的话同一个
        // 客户端在两种写法下会被算成两个 IP，限额就漏了
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
            other => other,
        };

        match address {
            IpAddr::V4(v4) => {
                let mut bytes = [0u8; 16];
                bytes[..4].copy_from_slice(&v4.octets());

                Self(bytes)
            }
            IpAddr::V6(v6) => {
                let mut bytes = v6.octets();
                bytes[8..].fill(0);

                Self(bytes)
            }
        }
    }
}

/// 这次连接该按哪个 IP 记额度。
///
/// `X-Forwarded-For` 只在**开了 `PAIR_TRUST_PROXY`** 且**对端本身是内网 / 回环地址**时
/// 才认：域名模式里中继只 `expose` 给 Docker 内网、前面站着 Caddy，那一跳可信；direct
/// 模式把端口发布到公网，那时对端是公网地址、整条头都不看——伪造不进来。
///
/// 取**最后一个**条目：那是我们信任的这一跳自己追加的（Caddy 是 append 而不是
/// replace），客户端伪造的前缀赢不了。
pub fn client_ip(peer: SocketAddr, forwarded_for: Option<&str>, trust_proxy: bool) -> IpKey {
    if trust_proxy && is_private_or_loopback(peer.ip()) {
        if let Some(address) = forwarded_for
            .and_then(|value| value.split(',').next_back())
            .and_then(|last| last.trim().parse::<IpAddr>().ok())
        {
            return IpKey::from_addr(address);
        }
    }

    IpKey::from_addr(peer.ip())
}

/// 私网 / 回环 / 链路本地。站在这些地址后面的一定是我们自己人（反代容器、docker 网桥、
/// 本机），所以只有那时才肯信它转发的头。
fn is_private_or_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_loopback())
        }
    }
}

/// 把 `iceServers` 过滤成「只剩 STUN」。
///
/// 公益档绝不能拿到 `turn:`（它按流量计费，比带宽贵得多），顺带把 `username` /
/// `credential` 也摘掉（纵深防御：TURN 凭据只对 TURN 条目有意义）。过滤完一条都不剩
/// 就返回 `None`——宁可不广告，也不要给出一份只有 TURN 的清单。
fn stun_only(servers: &serde_json::Value) -> Option<serde_json::Value> {
    let kept: Vec<serde_json::Value> = servers
        .as_array()?
        .iter()
        .filter(|entry| {
            urls_of(entry.get("urls").unwrap_or(&serde_json::Value::Null)).is_some_and(|urls| {
                !urls.is_empty() && urls.iter().all(|url| url.starts_with("stun:"))
            })
        })
        .map(|entry| serde_json::json!({ "urls": entry.get("urls").cloned().unwrap_or_default() }))
        .collect();

    (!kept.is_empty()).then_some(serde_json::Value::Array(kept))
}

/// `urls` 可以是单个字符串，也可以是字符串数组（与 WebRTC 的 `RTCIceServer` 一致）
fn urls_of(value: &serde_json::Value) -> Option<Vec<String>> {
    match value {
        serde_json::Value::String(url) => Some(vec![url.clone()]),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}

fn close_frame(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }
}

/// tungstenite 因为超过 `max_message_size` / `max_frame_size` 拒绝的帧，翻译成
/// 线上契约里的 `1009`（客户端据此把这次断开显示成「帧太大」）
fn too_large_close(error: &WsError) -> Option<CloseFrame> {
    matches!(
        error,
        WsError::Capacity(CapacityError::MessageTooLong { .. })
    )
    .then(|| close_frame(close_code::TOO_LARGE, "frame too large"))
}

/// 广播「某台设备离线了」——**只发给同一个 Room**（§15）。
///
/// 同一 deviceId 的新连接还在这个 Room 里时（顶替重连）这条是伪广播：对端会看到
/// 「上线 → 下线」，最终以为对方离线，所以要过滤掉。CF 版的 `announceOffline` 是
/// 同一套规则（靠 close 事件晚于 accept 达到同样效果）。
fn announce_offline(state: &mut State, room_id: &str, except: u64, device_id: String) {
    if is_false_offline(state, room_id, &device_id) {
        return;
    }

    broadcast(
        state,
        room_id,
        except,
        ServerFrame::Peer {
            online: false,
            device_id,
        },
    );
}

/// 同一 deviceId 的连接还在**这个 Room** 里时（顶替重连），这条离线通知是伪广播
fn is_false_offline(state: &State, room_id: &str, device_id: &str) -> bool {
    state
        .rooms
        .get(room_id)
        .is_some_and(|room| room.clients.contains_key(device_id))
}

/// 把一条控制帧广播给**同一个 Room** 里 `except` 之外的连接。
///
/// 用 `try_send` 而不是 `send().await`：这里在锁内，等空位会把整个会话层卡住。
/// 投不进去说明那条连接已经停摆（出站队列满），把它从名单里摘掉——留在里面会让
/// 另一方永远以为它在线，而它其实什么也收不到。
fn broadcast(state: &mut State, room_id: &str, except: u64, frame: ServerFrame) {
    let mut pending = vec![(room_id.to_string(), except, frame.to_json())];

    while let Some((room_id, except, json)) = pending.pop() {
        let mut stalled = Vec::new();

        if let Some(room) = state.rooms.get(&room_id) {
            for (device_id, entry) in room.clients.iter().filter(|(_, entry)| entry.id != except) {
                if entry
                    .sender
                    .try_send(Message::Text(json.clone().into()))
                    .is_err()
                {
                    stalled.push(device_id.clone());
                }
            }
        }

        for device_id in stalled {
            let Some(entry) = take_client(state, &room_id, &device_id) else {
                continue;
            };

            // 它一条通知都没收到，所以同 Room 剩下的人也必须知道它离线了。同一 deviceId 的
            // 新连接已经在列表里时（顶替重连），这条同样是伪通知，要一起过滤掉。
            if !is_false_offline(state, &room_id, &device_id) {
                pending.push((
                    room_id.clone(),
                    entry.id,
                    ServerFrame::Peer {
                        online: false,
                        device_id,
                    }
                    .to_json(),
                ));
            }

            // 摘掉最后一个连接之后这个 Room 就空了：名额要跟着还回去
            sweep(state, &room_id);
        }
    }
}

/// 把一条连接从所属 Room 里摘下来（顶替 / 停摆 / 正常断开共用）。
///
/// 顺便丢掉它的限流桶：已经不在名单里的连接不该再留桶。
fn take_client(state: &mut State, room_id: &str, device_id: &str) -> Option<ClientEntry> {
    let room = state.rooms.get_mut(room_id)?;
    let entry = room.clients.remove(device_id)?;

    state.buckets.remove(&entry.id);

    Some(entry)
}

/// 还回一份没用掉的入场许可。Room 空了就跟着删掉，把名额让出来（§13）。
fn release_pending(state: &mut State, room_id: &str) {
    let Some(room) = state.rooms.get_mut(room_id) else {
        return;
    };

    room.pending = room.pending.saturating_sub(1);

    sweep(state, room_id);
}

/// 一个人都不剩的 Room 就地删除——`PAIR_MAX_SESSIONS` 的名额随之释放（§13）。
///
/// **还在握手里的 Room 不能删**（`pending > 0`）：那份许可已经算进容量了，删掉它会让
/// 随后到达的 `admit` 落进「Room 不存在」的分支，两边的账就对不上了。
fn sweep(state: &mut State, room_id: &str) {
    let Some(room) = state.rooms.get(room_id) else {
        return;
    };

    if !room.clients.is_empty() || room.pending > 0 {
        return;
    }

    let created_at = room.created_at;
    let last_active = room.last_active;
    let tier = room.tier;
    let ip = room.ip;

    state.rooms.remove(room_id);

    // 公益档的每 IP 名额跟着房间走：房间空了就还回去，别让「开一组、马上退」把配额用光
    if tier == Tier::Public {
        if let Some(count) = state.public_rooms_per_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);

            if *count == 0 {
                state.public_rooms_per_ip.remove(&ip);
            }
        }
    }

    println!(
        "[{}] 双人会话已释放（{} 档，存活 {:.0}s，最后活动在 {:.0}s 前）",
        room_fingerprint(room_id),
        tier.as_str(),
        created_at.elapsed().as_secs_f64(),
        last_active.elapsed().as_secs_f64()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use tokio::sync::oneshot::error::TryRecvError;

    /// 固定向量用的配对密码（与 `crypto.rs` / `auth.rs` 里那份是同一个）
    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

    /// 三个互不相干的会话。中继本身不校验 `ROOM_ID` 的格式（那是 `server.rs` 的事），
    /// 所以这里用短名字就够了。
    const ROOM_A: &str = "room-a";
    const ROOM_B: &str = "room-b";
    const ROOM_C: &str = "room-c";

    /// 部署者那一档的服务器密码（会话层的默认档位）
    const SERVER_PASSWORD: &str = "relay-unit-tests-server-password";
    /// 公益档的服务器密码
    const PUBLIC_PASSWORD: &str = "relay-unit-tests-public-password";

    fn relay(max_sessions: usize, stale_after: Duration) -> Arc<Relay> {
        relay_with(Options {
            max_sessions,
            stale_after,
            ..Options::default()
        })
    }

    /// 只写出用例关心的那几项，其余走缺省——公益档一加，`RelayOptions` 就有十来项了
    #[derive(Default)]
    struct Options {
        max_sessions: usize,
        max_public_sessions: usize,
        max_public_per_ip: usize,
        public_window: Option<Duration>,
        stale_after: Duration,
        ice_servers: Option<serde_json::Value>,
        stun_port: Option<u16>,
        public_verifier: Option<[u8; 32]>,
        trust_proxy: bool,
    }

    fn relay_with(options: Options) -> Arc<Relay> {
        Relay::new(RelayOptions {
            limits: Limits::default(),
            public_limits: RelayOptions::default().public_limits,
            max_sessions: options.max_sessions,
            max_public_sessions: options.max_public_sessions,
            max_public_per_ip: options.max_public_per_ip,
            public_window: options.public_window,
            stale_after: options.stale_after,
            ice_servers: options.ice_servers,
            stun_port: options.stun_port,
            // 会话层用不到服务器密码（那是 `server.rs` 在升级之前判的），给一个固定摘要
            server_verifier: auth::server_verifier(SERVER_PASSWORD),
            public_verifier: options.public_verifier,
            trust_proxy: options.trust_proxy,
        })
    }

    /// 一个固定的来源 IP（IPv4）。同一个用例里所有连接默认都从它来。
    fn ip(last: u8) -> IpKey {
        IpKey::from_addr(IpAddr::from([10, 0, 0, last]))
    }

    /// R36：会话层不该认错的服务器凭据
    #[test]
    fn the_relay_only_accepts_its_own_server_password() {
        let relay = relay(20, Duration::from_secs(120));
        let token = auth::derive_server_token(SERVER_PASSWORD);

        assert_eq!(relay.classify_server_token(&token), Some(Tier::Full));
        assert_eq!(relay.classify_server_token(""), None);
        // 密码原文不是凭据：凭据是它派生出的一串
        assert_eq!(relay.classify_server_token(SERVER_PASSWORD), None);
        assert_eq!(
            relay.classify_server_token(&auth::derive_server_token("relay-unit-tests-server-password2")),
            None
        );
    }

    /// 两把钥匙各归各的档：这是「谁能用多少」的唯一判据
    #[test]
    fn the_two_server_passwords_map_to_their_own_tier() {
        let open = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        });

        assert_eq!(
            open.classify_server_token(&auth::derive_server_token(SERVER_PASSWORD)),
            Some(Tier::Full)
        );
        assert_eq!(
            open.classify_server_token(&auth::derive_server_token(PUBLIC_PASSWORD)),
            Some(Tier::Public)
        );
        assert_eq!(
            open.classify_server_token(&auth::derive_server_token("someone-else")),
            None
        );
        // 没配公益密码的部署：那把钥匙什么也不是，绝不能掉回部署者那一档
        let closed = relay(20, Duration::from_secs(120));

        assert_eq!(
            closed.classify_server_token(&auth::derive_server_token(PUBLIC_PASSWORD)),
            None
        );
        assert!(!closed.has_public_tier());
        assert!(open.has_public_tier());

        // 配了密码、但名额 0 = 这一档**实际关掉**（`PAIR_MAX_PUBLIC_SESSIONS=0`）。
        // 这时它必须表现得像「没有公益档」：那把钥匙进不来（403），而不是让每个连接
        // 拿到 503「会话已满」——那说的是一件没发生的事，客户端还会按退避一直重试。
        let off = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 0,
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        });

        assert_eq!(
            off.classify_server_token(&auth::derive_server_token(PUBLIC_PASSWORD)),
            None
        );
        assert!(!off.has_public_tier());
        // 部署者那一档不受影响
        assert_eq!(
            off.classify_server_token(&auth::derive_server_token(SERVER_PASSWORD)),
            Some(Tier::Full)
        );
    }

    /// 走完 `reserve` + `admit` 的正常路径（不 panic，拒绝的情况也返回给用例断言）
    async fn join(
        relay: &Arc<Relay>,
        room_id: &str,
        token: &str,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
    ) -> Admit {
        join_as(relay, room_id, token, device_id, sender, Tier::Full, ip(1)).await
    }

    /// 指定档位与来源 IP 的 `join`：公益档那几条用例要它（默认那条永远是部署者档）
    async fn join_tier(
        relay: &Arc<Relay>,
        room_id: &str,
        token: &str,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
        tier: Tier,
        source: IpKey,
    ) -> Result<Admit, RoomRejection> {
        let reservation = relay
            .reserve(room_id, auth::auth_verifier(token), tier, source)
            .await?;

        Ok(relay.admit(reservation, device_id, sender).await)
    }

    /// 走完 `reserve` + `admit` 的正常路径，`reserve` 被拒时 panic（返回拒绝原因的路走
    /// [`join_tier`]）
    async fn join_as(
        relay: &Arc<Relay>,
        room_id: &str,
        token: &str,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
        tier: Tier,
        source: IpKey,
    ) -> Admit {
        let reservation = relay
            .reserve(room_id, auth::auth_verifier(token), tier, source)
            .await
            .unwrap_or_else(|rejection| panic!("预留 {room_id} 不该被拒：{rejection:?}"));

        relay.admit(reservation, device_id, sender).await
    }

    /// `join` 的成功路径
    async fn join_ok(
        relay: &Arc<Relay>,
        room_id: &str,
        token: &str,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
    ) -> (u64, bool, oneshot::Receiver<()>) {
        match join(relay, room_id, token, device_id, sender).await {
            Admit::Accepted {
                id,
                peer_online,
                ejected,
            } => (id, peer_online, ejected),
            Admit::Full => panic!("{device_id} 不该被拒绝"),
        }
    }

    fn drain(receiver: &mut mpsc::Receiver<Message>) -> Vec<Message> {
        let mut messages = Vec::new();

        while let Ok(message) = receiver.try_recv() {
            messages.push(message);
        }

        messages
    }

    fn close_code_of(message: &Message) -> Option<u16> {
        match message {
            Message::Close(Some(frame)) => Some(frame.code.into()),
            _ => None,
        }
    }

    /// 队列里有没有「某人离线」的公告
    fn announced_offline(receiver: &mut mpsc::Receiver<Message>, device_id: &str) -> bool {
        drain(receiver).iter().any(|message| match message {
            Message::Text(text) => serde_json::from_str::<serde_json::Value>(text.as_str())
                .is_ok_and(|json| {
                    json["type"] == "server.peer"
                        && json["online"] == false
                        && json["deviceId"] == device_id
                }),
            _ => false,
        })
    }

    /// 队列里的第一条二进制帧（转发出去的正是它）
    fn next_binary(receiver: &mut mpsc::Receiver<Message>) -> Option<Vec<u8>> {
        drain(receiver)
            .into_iter()
            .find_map(|message| match message {
                Message::Binary(bytes) => Some(bytes.to_vec()),
                _ => None,
            })
    }

    #[test]
    fn a_message_beyond_the_websocket_layer_limit_maps_to_1009() {
        let too_long = WsError::Capacity(CapacityError::MessageTooLong {
            size: MAX_BINARY_FRAME_SIZE * 9,
            max_size: MAX_BINARY_FRAME_SIZE * 8,
        });
        let frame = too_large_close(&too_long).expect("应当映射成 1009");

        assert_eq!(u16::from(frame.code), close_code::TOO_LARGE);

        // 其它读错误（对端断开、协议错误）不在这里处理
        assert!(too_large_close(&WsError::ConnectionClosed).is_none());
    }

    #[test]
    fn the_bucket_holds_one_second_of_capacity() {
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::full(limits, start);
        let frame = 1_024.0;
        let mut admitted = 0;

        // 容量就是「每秒上限」：一直发到被拒为止，正好 30 帧
        while bucket.take(start, 1.0, 0.0, frame) {
            admitted += 1;
        }

        assert_eq!(admitted, 30);

        // 过一秒补满。注意上一次超限把桶扣成了 -1（与 CF 版一致：先扣再判，连接随即
        // 关闭所以不回滚），所以补满后能放行的是 29 帧。
        let later = start + Duration::from_secs(1);
        let mut admitted_after_refill = 0;

        while bucket.take(later, 1.0, 0.0, frame) {
            admitted_after_refill += 1;
        }

        assert_eq!(admitted_after_refill, 29);
    }

    #[test]
    fn a_twenty_chunk_burst_is_legal() {
        // 20 个 512 KiB chunk（含帧头与 nonce/tag 每个约 524 KiB）合计约 10 MiB，
        // 低于 12 MiB 的字节上限——这正是 12 MiB 这个数字的由来
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::full(limits, start);
        let chunk = 512.0 * 1024.0 + FRAME_HEADER_SIZE as f64 + 24.0 + 16.0;

        for _ in 0..20 {
            assert!(bucket.take(start, 1.0, 1.0, chunk));
        }

        // 第 21 个 chunk 会被分片额度拦住
        assert!(!bucket.take(start, 1.0, 1.0, chunk));
    }

    #[test]
    fn the_byte_bucket_is_twelve_mebibytes() {
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::full(limits, start);
        let half = limits.bytes_per_second / 2.0;

        assert!(bucket.take(start, 0.0, 0.0, half));
        assert!(bucket.take(start, 0.0, 0.0, half));
        assert!(!bucket.take(start, 0.0, 0.0, 1.0));
    }

    /// §30「Room 创建」：第一个人进来就把会话建起来，另一个人还没在
    #[tokio::test]
    async fn a_room_is_created_by_its_first_client() {
        let relay = relay(2, Duration::from_secs(120));
        let (sender, _receiver) = mpsc::channel(OUTBOUND_QUEUE);

        let (_, peer_online, _) = join_ok(&relay, ROOM_A, "token-a", "a1", &sender).await;

        assert!(!peer_online, "第一个人进来时对端还没上线");
    }

    /// §18 / §30「双人加入」：同一个 secret 派生出的 Room 与 token 会让两个人落在同一会话里；
    /// 另一个 secret 走的是**另一个** Room，而不是被当成第三台设备
    #[tokio::test]
    async fn the_same_secret_lands_in_the_same_room() {
        let relay = relay(4, Duration::from_secs(120));
        let secret = auth::decode_pair_secret(SECRET).unwrap();
        let room = auth::derive_room_id(&secret);
        let token = auth::derive_auth_token(&secret);
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b1_tx, _b1_rx) = mpsc::channel(OUTBOUND_QUEUE);

        // 真实形态：32 字节 base64url 无填充
        assert_eq!(room.len(), 43);

        let (_, first_online, _) = join_ok(&relay, &room, &token, "a1", &a1_tx).await;

        assert!(!first_online);

        let (_, second_online, _) = join_ok(&relay, &room, &token, "a2", &a2_tx).await;

        assert!(second_online, "同一个密钥的第二个人应当看到对端在线");

        let other_secret = [7u8; auth::PAIR_SECRET_BYTES];
        let other_room = auth::derive_room_id(&other_secret);
        let other_token = auth::derive_auth_token(&other_secret);
        let (_, other_online, _) = join_ok(&relay, &other_room, &other_token, "b1", &b1_tx).await;

        assert_ne!(other_room, room);
        assert!(!other_online, "另一个会话的第一个人也不该看到对端在线");
    }

    /// §30「第三台设备」：同一个 Room 里的第三台不同设备进不来，但**别的** Room 不受影响
    #[tokio::test]
    async fn the_third_device_of_a_room_is_rejected_while_other_rooms_are_not() {
        let relay = relay(4, Duration::from_secs(120));
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a3_tx, _a3_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b1_tx, _b1_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;
        join_ok(&relay, ROOM_A, "token-a", "a2", &a2_tx).await;

        assert!(matches!(
            join(&relay, ROOM_A, "token-a", "a3", &a3_tx).await,
            Admit::Full
        ));

        // 别的会话一点都没被牵连
        let (_, peer_online, _) = join_ok(&relay, ROOM_B, "token-b", "b1", &b1_tx).await;

        assert!(!peer_online);
    }

    /// §9 / §30「Secret 错误」：同名 Room 上拿错 token 必须 401，而且**不能**建出第二个房间
    #[tokio::test]
    async fn a_wrong_token_on_an_existing_room_is_refused() {
        let relay = relay(1, Duration::from_secs(120));
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;

        assert_eq!(
            relay
                .reserve(ROOM_A, auth::auth_verifier("token-wrong"), Tier::Full, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::AuthMismatch
        );

        // 被拒之后这个房间还是原来那个（没被清掉、也没被替换）
        let (_, peer_online, _) = join_ok(&relay, ROOM_A, "token-a", "a2", &a2_tx).await;

        assert!(peer_online);
    }

    /// §11：同一个 deviceId 重连顶替旧连接，不能算第三个人，也不该发伪离线公告
    #[tokio::test]
    async fn reconnecting_with_the_same_device_id_replaces_without_a_false_offline() {
        let relay = relay(2, Duration::from_secs(120));
        let (a1_tx, mut a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, mut a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a", &a1_tx).await;
        join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        let (_, peer_online, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a2_tx).await;

        assert!(peer_online, "同一个 deviceId 重连不能被当成第三个人");
        assert_eq!(
            drain(&mut a1_rx).iter().filter_map(close_code_of).next(),
            Some(close_code::REPLACED)
        );
        assert!(drain(&mut a2_rx).is_empty(), "新连接不该收到关闭帧");

        // B 只看到 A 上线的通知（第二条），不该看到「A 下线」
        let seen = drain(&mut b_rx);

        assert_eq!(seen.iter().filter_map(close_code_of).count(), 0);
        assert!(!announced_offline(&mut b_rx, "a"), "不该有伪离线");
    }

    /// §12：陈旧连接被顶替（4004），并且只在这个 Room 里发生
    #[tokio::test]
    async fn a_stale_peer_is_evicted_with_4004() {
        let relay = relay(2, Duration::ZERO);
        let (a_tx, mut a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        drain(&mut a_rx);
        drain(&mut b_rx);

        tokio::time::sleep(Duration::from_millis(5)).await;

        let (_, peer_online, _) = join_ok(&relay, ROOM_A, "token-a", "c", &c_tx).await;

        assert!(peer_online, "顶替之后还剩一个对端");

        // 顶替的是先连进来的那个
        assert_eq!(
            drain(&mut a_rx).iter().filter_map(close_code_of).next(),
            Some(close_code::STALE)
        );

        // 活下来的那个收到「a 离线」再收到「c 上线」，而不是关闭帧
        let seen: Vec<serde_json::Value> = drain(&mut b_rx)
            .iter()
            .map(|message| serde_json::from_str(message.to_text().unwrap()).unwrap())
            .collect();

        assert_eq!(seen.len(), 2, "实际：{seen:?}");
        assert_eq!(seen[0]["type"], "server.peer");
        assert_eq!(seen[0]["online"], false);
        assert_eq!(seen[0]["deviceId"], "a");
        assert_eq!(seen[1]["type"], "server.peer");
        assert_eq!(seen[1]["online"], true);
        assert_eq!(seen[1]["deviceId"], "c");
    }

    #[tokio::test]
    async fn dropping_a_peer_announces_offline_to_the_other_side() {
        let relay = relay(2, Duration::from_secs(120));
        let (a_tx, mut a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;

        let (b_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        drain(&mut a_rx);
        relay.drop_peer(ROOM_A, "b", b_id).await;

        let seen = drain(&mut a_rx);
        let json: serde_json::Value = serde_json::from_str(seen[0].to_text().unwrap()).unwrap();

        assert_eq!(json["type"], "server.peer");
        assert_eq!(json["online"], false);
        assert_eq!(json["deviceId"], "b");
    }

    /// §15 / §30「Room 隔离」：A 房发的帧只有 A 房的另一个人收到，必须做负向断言
    #[tokio::test]
    async fn binary_frames_never_cross_rooms() {
        let relay = relay(4, Duration::from_secs(120));
        let (a1_tx, mut a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, mut a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b1_tx, mut b1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b2_tx, mut b2_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (a1_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;

        join_ok(&relay, ROOM_A, "token-a", "a2", &a2_tx).await;
        join_ok(&relay, ROOM_B, "token-b", "b1", &b1_tx).await;
        join_ok(&relay, ROOM_B, "token-b", "b2", &b2_tx).await;

        drain(&mut a1_rx);
        drain(&mut a2_rx);
        drain(&mut b1_rx);
        drain(&mut b2_rx);

        relay
            .forward(ROOM_A, a1_id, Message::Binary(vec![9u8; 32].into()))
            .await;

        assert_eq!(next_binary(&mut a2_rx), Some(vec![9u8; 32]));
        // 负向断言：B 房一条都收不到，发送者自己也不回环
        assert!(next_binary(&mut b1_rx).is_none(), "B 房收到了 A 房的帧");
        assert!(next_binary(&mut b2_rx).is_none(), "B 房收到了 A 房的帧");
        assert!(next_binary(&mut a1_rx).is_none(), "不该回发给发送者");
    }

    /// §30「server.peer 隔离」：上下线公告不能跨 Room
    #[tokio::test]
    async fn peer_announcements_stay_inside_the_room() {
        let relay = relay(4, Duration::from_secs(120));
        let (a1_tx, mut a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b1_tx, mut b1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b2_tx, mut b2_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;

        let (a2_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a2", &a2_tx).await;

        join_ok(&relay, ROOM_B, "token-b", "b1", &b1_tx).await;
        join_ok(&relay, ROOM_B, "token-b", "b2", &b2_tx).await;

        drain(&mut a1_rx);
        drain(&mut b1_rx);
        drain(&mut b2_rx);

        relay.drop_peer(ROOM_A, "a2", a2_id).await;

        assert!(announced_offline(&mut a1_rx, "a2"), "同房的人应当收到离线");
        assert!(drain(&mut b1_rx).is_empty(), "B 房收到了 A 房的离线公告");
        assert!(drain(&mut b2_rx).is_empty(), "B 房收到了 A 房的离线公告");
    }

    /// §8 / §27 / §30「已有 Room 不受容量影响」：满员只挡**新建**会话
    #[tokio::test]
    async fn capacity_only_blocks_new_rooms() {
        let relay = relay(2, Duration::from_secs(120));
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b1_tx, _b1_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;
        join_ok(&relay, ROOM_B, "token-b", "b1", &b1_tx).await;

        // 新的会话被拒（就是 server.rs 翻成 HTTP 503 的那条路径）
        assert_eq!(
            relay
                .reserve(ROOM_C, auth::auth_verifier("token-c"), Tier::Full, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::Capacity
        );

        // 已存在的会话里，第二个人照样能进
        let (_, peer_online, _) = join_ok(&relay, ROOM_A, "token-a", "a2", &a2_tx).await;

        assert!(peer_online);

        // 同 deviceId 重连也不受影响（用的是同一个 Room 的第二条连接）
        let (_, _, _) = join_ok(&relay, ROOM_A, "token-a", "a1", &a2_tx).await;
    }

    /// §13 / §30「capacity 释放」：最后一个客户端走了，会话被删除，名额让给新会话
    #[tokio::test]
    async fn a_room_that_empties_frees_its_slot() {
        let relay = relay(1, Duration::from_secs(120));
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (a1_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a1", &a1_tx).await;

        assert_eq!(
            relay
                .reserve(ROOM_B, auth::auth_verifier("token-b"), Tier::Full, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::Capacity
        );

        relay.drop_peer(ROOM_A, "a1", a1_id).await;

        assert!(
            relay
                .reserve(ROOM_B, auth::auth_verifier("token-b"), Tier::Full, ip(1))
                .await
                .is_ok(),
            "会话空了就该把名额还回来"
        );
    }

    /// §14 / §30「disconnect race」：旧连接的迟到清理不能把新连接删掉
    #[tokio::test]
    async fn a_late_cleanup_from_a_replaced_connection_keeps_the_new_one() {
        let relay = relay(2, Duration::from_secs(120));
        let (a1_tx, mut a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, mut a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (a1_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a1_tx).await;
        let (b_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;
        let (a2_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a2_tx).await;

        assert_ne!(a1_id, a2_id);

        drain(&mut a1_rx);
        drain(&mut a2_rx);
        drain(&mut b_rx);

        // 旧 socket 的读循环结束得比顶替晚：这条清理必须被识别成「名单里那条不是我」
        relay.drop_peer(ROOM_A, "a", a1_id).await;

        // 新连接还在：B 发的帧必须送到它手上
        relay
            .forward(ROOM_A, b_id, Message::Binary(vec![5u8; 8].into()))
            .await;

        assert_eq!(next_binary(&mut a2_rx), Some(vec![5u8; 8]));
        // 而且不能因此冒出「a 离线」这种伪公告
        assert!(!announced_offline(&mut b_rx, "a"));
    }

    /// 握手失败要还名额：`release` 之后那个会话就不该再占位置
    #[tokio::test]
    async fn a_reservation_that_never_reaches_admit_is_given_back() {
        let relay = relay(1, Duration::from_secs(120));
        let reservation = relay
            .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, ip(1))
            .await
            .unwrap();

        relay.release(reservation).await;

        assert!(
            relay
                .reserve(ROOM_B, auth::auth_verifier("token-b"), Tier::Full, ip(1))
                .await
                .is_ok(),
            "没用掉的预留必须把名额还回来"
        );
    }

    /// 还有连接在握手里的会话不能被清掉：那份预留已经算进容量了
    #[tokio::test]
    async fn a_room_with_a_handshake_in_flight_is_not_swept() {
        let relay = relay(1, Duration::from_secs(120));
        let first = relay
            .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, ip(1))
            .await
            .unwrap();
        let second = relay
            .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, ip(1))
            .await
            .unwrap();

        relay.release(first).await;

        // 房间还在，而且仍然认同一份密钥（没有被清掉再重建）
        assert_eq!(
            relay
                .reserve(ROOM_A, auth::auth_verifier("wrong"), Tier::Full, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::AuthMismatch
        );

        let (sender, _receiver) = mpsc::channel(OUTBOUND_QUEUE);

        assert!(matches!(
            relay.admit(second, "a1", &sender).await,
            Admit::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn the_rate_limit_is_per_socket() {
        let relay = relay(2, Duration::from_secs(120));
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (id_a, _, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        let (id_b, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        for _ in 0..30 {
            assert!(relay.allow(id_a, 1.0, 0.0, 64.0).await);
        }
        assert!(!relay.allow(id_a, 1.0, 0.0, 64.0).await);

        // 另一个 socket 有自己的桶
        assert!(relay.allow(id_b, 1.0, 0.0, 64.0).await);

        // 已经摘掉的连接不再有桶，也不会被 `entry().or_insert_with()` 重新造出来
        relay.drop_peer(ROOM_A, "b", id_b).await;
        assert!(!relay.allow(id_b, 1.0, 0.0, 64.0).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_peer_is_ejected_after_the_forward_timeout() {
        let relay = relay(2, Duration::from_secs(120));
        // A 的接收端一直活着但不消费：队列会满，`send` 会一直等空位
        let (a_tx, mut a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (_, _, mut ejected) = join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        let (b_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        drain(&mut a_rx);
        drain(&mut b_rx);

        let frame = Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into());

        for _ in 0..OUTBOUND_QUEUE {
            relay.forward(ROOM_A, b_id, frame.clone()).await;
        }

        // 队列满了，这一帧要等满 FORWARD_TIMEOUT 才会被放弃（`start_paused` 会自动
        // 把时钟推到那个定时器）
        relay.forward(ROOM_A, b_id, frame).await;

        // 摘牌之后 A 的读循环必须能收到信号，否则它的 socket 会一直挂着
        assert!(matches!(ejected.try_recv(), Err(TryRecvError::Closed)));

        // 配对位也空出来了：第三个人进来不该被当成第三人
        assert!(matches!(
            join(&relay, ROOM_A, "token-a", "c", &c_tx).await,
            Admit::Accepted { .. }
        ));

        // B 也收到了「A 离线」，不会一直以为对方在线
        assert!(announced_offline(&mut b_rx, "a"));
    }

    #[tokio::test]
    async fn an_observer_that_cannot_be_reached_is_evicted() {
        let relay = relay(2, Duration::from_secs(120));
        let (a_tx, mut a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (_, _, mut ejected) = join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        let (b_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        drain(&mut a_rx);
        drain(&mut b_rx);

        let frame = Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into());

        for _ in 0..OUTBOUND_QUEUE {
            relay.forward(ROOM_A, b_id, frame.clone()).await;
        }

        // B 断开时的离线通知投不进 A（队列满）：A 必须被摘掉，而不是被静默跳过
        relay.drop_peer(ROOM_A, "b", b_id).await;

        assert!(matches!(ejected.try_recv(), Err(TryRecvError::Closed)));
        assert!(matches!(
            join(&relay, ROOM_A, "token-a", "c", &c_tx).await,
            Admit::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn forwarding_to_a_closed_queue_drops_the_stalled_peer() {
        let relay = relay(2, Duration::from_secs(120));
        let (a_tx, a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, mut b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (id_a, _, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        let (b_id, _, _) = join_ok(&relay, ROOM_A, "token-a", "b", &b_tx).await;

        // 让 A 的连接「死掉」：接收端被丢掉，转发时会立刻发现 channel 关闭
        drop(a_rx);
        drain(&mut b_rx);

        relay
            .forward(
                ROOM_A,
                b_id,
                Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into()),
            )
            .await;

        // A 被摘掉后，配对位空出来了：第三个人进来不该被当成第三人
        assert!(matches!(
            join(&relay, ROOM_A, "token-a", "c", &c_tx).await,
            Admit::Accepted { .. }
        ));
        assert_ne!(id_a, b_id);

        // 而且 B 收到了「A 离线」，不会一直以为对方在线
        assert!(announced_offline(&mut b_rx, "a"));
    }

    // -----------------------------------------------------------------------
    // 公益档：名额 / 档位 / 每 IP 限额 / 只给 STUN
    // -----------------------------------------------------------------------

    fn public_relay(
        max_sessions: usize,
        max_public_sessions: usize,
        max_public_per_ip: usize,
    ) -> Arc<Relay> {
        relay_with(Options {
            max_sessions,
            max_public_sessions,
            max_public_per_ip,
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        })
    }

    /// 两档各算各的名额：公益档再热闹也占不到部署者自己的位置，反之亦然
    #[tokio::test]
    async fn the_public_tier_has_its_own_capacity() {
        let relay = public_relay(1, 1, 0);
        let (public_tx, _public_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (full_tx, _full_rx) = mpsc::channel(OUTBOUND_QUEUE);

        // 公益档占掉它那唯一一个位置
        join_as(
            &relay,
            ROOM_A,
            "token-a",
            "a1",
            &public_tx,
            Tier::Public,
            ip(1),
        )
        .await;

        // 公益档满了，但它占**不到**部署者那一档的位置
        join_as(
            &relay,
            ROOM_B,
            "token-b",
            "b1",
            &full_tx,
            Tier::Full,
            ip(1),
        )
        .await;

        // 于是两档各拒各的：两边都是「自己那一档满了」
        assert_eq!(
            relay
                .reserve(
                    &room_id_of('c'),
                    auth::auth_verifier("token-c"),
                    Tier::Public,
                    ip(1)
                )
                .await
                .unwrap_err(),
            RoomRejection::Capacity
        );
        assert_eq!(
            relay
                .reserve(
                    &room_id_of('d'),
                    auth::auth_verifier("token-d"),
                    Tier::Full,
                    ip(2)
                )
                .await
                .unwrap_err(),
            RoomRejection::Capacity
        );
    }

    /// 一个合法的 Room 名字（`reserve` 只把它当分组键，不校验格式）
    fn room_id_of(seed: char) -> String {
        format!("room-{seed}{seed}{seed}")
    }

    /// 档位定在 Room 上：同一个会话的两个人必须填同一类密码
    #[tokio::test]
    async fn a_room_keeps_the_tier_it_was_created_with() {
        let relay = public_relay(20, 20, 0);
        let (public_tx, _public_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (full_tx, _full_rx) = mpsc::channel(OUTBOUND_QUEUE);

        join_as(
            &relay,
            ROOM_A,
            "token-a",
            "a1",
            &public_tx,
            Tier::Public,
            ip(1),
        )
        .await;

        // 同一个人第二个连接、拿另一类密码：拒（填错密码的人不该进来，
        // 而不是「一个人有中继兜底、另一个人什么都没有」）
        assert_eq!(
            relay
                .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::TierMismatch
        );

        // 而同一类密码的第二个人照旧进得来
        assert!(matches!(
            join_as(
                &relay,
                ROOM_A,
                "token-a",
                "a2",
                &full_tx,
                Tier::Public,
                ip(1)
            )
            .await,
            Admit::Accepted { peer_online: true, .. }
        ));
    }

    /// 公益档的每 IP 限额只挡**新建会话**，而且会话空了就把账还回去
    #[tokio::test]
    async fn the_public_per_ip_limit_counts_rooms_and_gives_them_back() {
        let relay = public_relay(20, 20, 1);
        let (a1_tx, _a1_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (a2_tx, _a2_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);

        let (a1_id, _, _) =
            join_ok_tier(&relay, ROOM_A, "token-a", "a1", &a1_tx, Tier::Public).await;

        // 同一个会话的第二个人不受影响（同一个 NAT 下面的一对用户不能被自己挡住）
        let (a2_id, peer_online, _) =
            join_ok_tier(&relay, ROOM_A, "token-a", "a2", &a2_tx, Tier::Public).await;

        assert!(peer_online);

        // 第二个**会话**来自同一个 IP：拒
        assert_eq!(
            relay
                .reserve(ROOM_B, auth::auth_verifier("token-b"), Tier::Public, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::PublicIpLimit
        );

        // 另一个 IP 不受影响
        assert!(
            join_tier(&relay, ROOM_B, "token-b", "b1", &b_tx, Tier::Public, ip(2))
                .await
                .is_ok()
        );

        // 第一个会话空了之后，同一个 IP 又能开一个（账要还回去）
        relay.drop_peer(ROOM_A, "a1", a1_id).await;
        relay.drop_peer(ROOM_A, "a2", a2_id).await;

        let room_a = state_rooms(&relay).await;

        assert!(!room_a.contains(&ROOM_A.to_string()), "会话空了就该被清掉");
        assert!(
            join_tier(&relay, ROOM_C, "token-c", "c1", &c_tx, Tier::Public, ip(1))
                .await
                .is_ok()
        );
    }

    /// `join` 的档位版成功路径
    async fn join_ok_tier(
        relay: &Arc<Relay>,
        room_id: &str,
        token: &str,
        device_id: &str,
        sender: &mpsc::Sender<Message>,
        source_tier: Tier,
    ) -> (u64, bool, oneshot::Receiver<()>) {
        match join_as(
            relay,
            room_id,
            token,
            device_id,
            sender,
            source_tier,
            ip(1),
        )
        .await
        {
            Admit::Accepted {
                id,
                peer_online,
                ejected,
            } => (id, peer_online, ejected),
            Admit::Full => panic!("{device_id} 不该被拒绝"),
        }
    }

    /// 当前活着的会话名（只给用例断言用）
    async fn state_rooms(relay: &Arc<Relay>) -> Vec<String> {
        relay
            .state
            .lock()
            .await
            .rooms
            .keys()
            .cloned()
            .collect()
    }

    /// IPv6 按 /64 归并：同一段里的地址算同一个 IP，别的段算另一个
    #[test]
    fn ip_keys_fold_ipv6_by_slash_64() {
        let first: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let same_prefix: IpAddr = "2001:db8:1:2:ffff::1".parse().unwrap();
        let other_prefix: IpAddr = "2001:db8:1:3::1".parse().unwrap();

        assert_eq!(IpKey::from_addr(first), IpKey::from_addr(same_prefix));
        assert_ne!(IpKey::from_addr(first), IpKey::from_addr(other_prefix));

        // IPv4 按 /32：同一段的邻居是**另一个** IP
        assert_ne!(
            IpKey::from_addr("10.0.0.1".parse().unwrap()),
            IpKey::from_addr("10.0.0.2".parse().unwrap())
        );

        // 双栈 socket 上的 IPv4 客户端以 `::ffff:a.b.c.d` 出现，要和纯 IPv4 算成同一个
        assert_eq!(
            IpKey::from_addr("::ffff:10.0.0.1".parse().unwrap()),
            IpKey::from_addr("10.0.0.1".parse().unwrap())
        );
    }

    /// `X-Forwarded-For` 只在「开了开关」且「对端本身是自己人」时才认
    #[test]
    fn client_ip_only_trusts_forwarded_for_from_a_private_peer() {
        let private_peer: SocketAddr = "172.18.0.5:5000".parse().unwrap();
        let public_peer: SocketAddr = "203.0.113.9:5000".parse().unwrap();

        // 不开开关：永远看 peer
        assert_eq!(
            client_ip(private_peer, Some("198.51.100.7"), false),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );

        // 开了开关 + 私网 peer：取**最后一个**（可信那一跳自己追加的）
        assert_eq!(
            client_ip(private_peer, Some("198.51.100.7, 203.0.113.9"), true),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );

        // 公网 peer 伪造 XFF：整条头都不看
        assert_eq!(
            client_ip(public_peer, Some("198.51.100.7"), true),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );

        // 畸形 / 缺失：退回 peer，绝不能因此少算一个 IP（限额会漏）
        assert_eq!(
            client_ip(private_peer, Some("not-an-ip"), true),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );
        assert_eq!(
            client_ip(private_peer, None, true),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );
    }

    /// 公益档只拿 `stun:`：`turn:` 条目与凭据一个都不给
    #[test]
    fn the_public_tier_only_gets_stun_servers() {
        let servers = serde_json::json!([
            { "urls": ["stun:cat.example.com:3478"] },
            {
                "urls": ["turn:cat.example.com:3478"],
                "username": "coturn-user",
                "credential": "coturn-pass"
            },
            { "urls": ["turn:cat.example.com:3478?transport=tcp"] }
        ]);
        let relay = relay_with(Options {
            max_sessions: 20,
            ice_servers: Some(servers.clone()),
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        });

        assert_eq!(
            relay.ice_servers_for(Some("cat.example.com:8080"), Tier::Full),
            Some(servers)
        );
        assert_eq!(
            relay.ice_servers_for(Some("cat.example.com:8080"), Tier::Public),
            Some(serde_json::json!([{ "urls": ["stun:cat.example.com:3478"] }]))
        );

        // 只有 TURN 的部署配置：公益档宁可不广告，也不给一份带凭据的清单
        let turn_only = relay_with(Options {
            max_sessions: 20,
            ice_servers: Some(serde_json::json!([{
                "urls": ["turn:cat.example.com:3478"],
                "username": "u",
                "credential": "p"
            }])),
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        });

        assert!(turn_only
            .ice_servers_for(Some("cat.example.com:8080"), Tier::Public)
            .is_none());

        // 内置 STUN 那一条路两档一样（STUN 本来就不鉴权）
        let builtin = relay_with(Options {
            max_sessions: 20,
            stun_port: Some(3479),
            public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
            ..Options::default()
        });

        assert_eq!(
            builtin.ice_servers_for(Some("cat.example.com:8080"), Tier::Public),
            Some(serde_json::json!([{ "urls": ["stun:cat.example.com:3479"] }]))
        );
    }

    /// 公益档的额度是另一份（默认 10 帧/秒 / 256 KiB/秒），桶按档位取
    #[test]
    fn the_public_tier_uses_its_own_limits() {
        let relay = public_relay(20, 20, 0);
        let start = Instant::now();
        let public = Bucket::full(relay.limits_for(Tier::Public), start);
        let full = Bucket::full(relay.limits_for(Tier::Full), start);

        assert_eq!(
            public.limits.frames_per_second,
            protocol::DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND
        );
        assert_eq!(
            public.limits.bytes_per_second,
            protocol::DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND
        );
        assert!(
            public.limits.frames_per_second < full.limits.frames_per_second,
            "公益档的额度必须比部署者那一档小"
        );
    }
}

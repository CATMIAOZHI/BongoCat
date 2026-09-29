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

/// 公益档连接的 **WebSocket 层**读上限。
///
/// 协议上限是 [`MAX_PUBLIC_FRAME_SIZE`]，但交给 tungstenite 的 `max_message_size` 不能
/// 贴着它设：超限时 tungstenite 读完**帧头**就报错，帧体还在接收缓冲里，关连接会让 TCP
/// 直接 RST，对端看到的是「连接被重置」而不是干净的 `1009`（完全档那边的 8 MiB 余量就是
/// 同一个理由）。取两倍：64 KiB ~ 128 KiB 的帧能被完整读完、干净地回 `1009`，再大就落到
/// 「尽力回 `1009`」那条路径。
///
/// 它的真正作用是**内存隔离**：这一条把单条公益连接的读缓冲从完全档的 8 MiB 压到 128 KiB，
/// 于是「一堆公益连接把宿主机内存吃光、把部署者自己那一档一起搞死」这条路被堵住。
pub const PUBLIC_WS_MESSAGE_SIZE: usize = 2 * MAX_PUBLIC_FRAME_SIZE;

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

/// 公益档一条连接的**持续**额度（`PAIR_PUBLIC_MAX_FRAMES_PER_SECOND` /
/// `PAIR_PUBLIC_MAX_BYTES_PER_SECOND`）——这是回填速率，不再是「突发容量」（见下）。
///
/// 诚实形状是它唯一的依据：一组一轮打洞是 `hello` + `offer`/`answer` + 逐条的 `candidate`
/// （一条一帧），**约 12 帧、8 KB**，一次发完，然后长时间静默（失败时按 5 → 120 秒退避
/// 重试一轮，整夜也就是 0.2~1.6 KB/秒）。所以持续额度压到 12 帧/秒与 **8 KiB/秒** 仍然有
/// 5~35 倍余量，而「拿 kind 8 夹带数据」被压到 28 MB/小时这个量级。
///
/// 帧这一维还会被客户端看到：它按广告值的 2/3 推导自己的出站速率（12 → 8 帧/秒），
/// 12 帧的报价因此摊在约 1.5 秒里发完——打洞不差这点时间。字节那一维客户端不消费
/// （它只用帧与分片两维），所以它纯粹是中继自己的账。
pub const DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND: f64 = 12.0;
pub const DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND: f64 = 8.0 * 1024.0;

/// 公益档一条连接的**突发容量**（`PAIR_PUBLIC_BURST_FRAMES` / `PAIR_PUBLIC_BURST_BYTES`）。
///
/// 令牌桶的容量与回填速率**必须是两个数**：只用一个数时它就是「容量 = 1 秒的量」，于是
/// 要么小到装不下一轮打洞（误伤），要么大到等于没有上限（比如「256 KiB/秒」实际是
/// 「256 KiB 的突发」，一小时能灌 900 MB）。诚实形状偏偏是「一轮几 KB 的突发 + 长期
/// 静默」，所以这里把突发给足、把持续压低：
///
/// - 24 帧 / 64 KiB 的突发：一轮 12 帧 / 8 KB 能一口气发完（2 倍 / 8 倍余量）；
/// - 上面那两个持续值：整夜重试也不会见底（见它的注释）。
pub const DEFAULT_PUBLIC_BURST_FRAMES: f64 = 24.0;
pub const DEFAULT_PUBLIC_BURST_BYTES: f64 = 64.0 * 1024.0;

/// 公益档**每把钥匙**的滚动预算（`PAIR_PUBLIC_KEY_BUDGET_BYTES`，0 = 不设这一层）。
///
/// 记账挂到「钥匙」上，而不是连接 / 房间 / IP：连接与房间都能靠「断开重连」「换个配对
/// 密码」白嫖刷新（Room 是客户端自己用配对密码推出来的，一空就被清掉），IP 会连坐同一个
/// NAT 下的无辜用户、在 IPv6 上还很软——而**钥匙是部署者发出去的**，拿钥匙的人换不掉。
///
/// 16 MiB 的桶 + 每小时回填 16 MiB：诚实用法每把钥匙约 0.8 MB/小时（整夜打不通的极端值），
/// 20 倍余量；而夹带者最坏也只能「先花 16 MiB、之后 16 MiB/小时」——想更多就得换钥匙，
/// 那件事只有部署者做得到。它同时也是「谁在夹带」的天然身份（详见 `sweep` 那行日志）。
pub const DEFAULT_PUBLIC_KEY_BUDGET_BYTES: f64 = 16.0 * 1024.0 * 1024.0;

/// 完全档**每把钥匙**的滚动预算（`PAIR_FULL_KEY_BUDGET_BYTES`，0 = 不设这一层）。
///
/// 与公益档那一份是同一套记账（都挂在**钥匙**上，见 `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`），
/// 只是额度大得多：这一档要承载聊天、语音与附件分片，诚实用量本来就比「一轮打洞」大几个
/// 数量级。它防的不是「你自己用超」，而是**一把流出去的钥匙**：在被撤销之前，那把钥匙最多
/// 先花 2 GiB、之后每小时 2 GiB——再想多就得换一把，而那件事只有部署者做得到。
///
/// 2 GiB 的取法是「比任何诚实的一小时都宽、比一条被滥用的千兆链路窄得多」：按 2 GiB/小时
/// 算，持续速率约 4.7 Mbit/s，已经接近这类廉价云主机的带宽上限，所以它对诚实用法是隐形的，
/// 对「拿它当免费中转」是硬的。设 `0` = 不设这一层（回到从前那样无限）。
pub const DEFAULT_FULL_KEY_BUDGET_BYTES: f64 = 2.0 * 1024.0 * 1024.0 * 1024.0;

/// 公益档的空闲回收窗口（`PAIR_PUBLIC_WINDOW_SECS`）。
///
/// **它是空闲回收器，不是「打洞截止时间」**：中继看不到 DataChannel 有没有建立成功
/// （信令是密文），所以任何「到点硬断」都会掐断**已经直连成功、正在正常使用**的会话
/// ——而中继一断，客户端是整条会话重启、直连也跟着重来。这里的判据是「多久没收到**任何**
/// 入站消息」，诚实客户端每 60 秒发一次 WebSocket Ping，180 秒 = 三次漏拍。
pub const DEFAULT_PUBLIC_WINDOW_SECS: u64 = 180;

/// 完全档的空闲回收窗口（`PAIR_FULL_WINDOW_SECS`，0 = 不回收）。
///
/// 判据与公益档那条完全一样（见 `DEFAULT_PUBLIC_WINDOW_SECS`），只是窗口更长：这一档是
/// 部署者自己与朋友的日常会话，不该被一个偏紧的秒数打断。它要解决的是**僵尸连接**——对端
/// 机器睡眠、网线被拔之后，TCP 可能几小时都不报错，于是一条什么都没在传的连接会一直占着
/// 会话名额与一条连接许可（`stale_after` 只能让**同一个 deviceId** 的新连接顶替它，救不了
/// 「人已经不在了」这种）。诚实客户端每 60 秒一次 WebSocket Ping，300 秒 = 五次漏拍。
pub const DEFAULT_FULL_WINDOW_SECS: u64 = 300;

/// 同一把服务器钥匙最多能同时开几组会话（`PAIR_MAX_SESSIONS_PER_KEY`，0 = 不限）。
///
/// 「一把钥匙一个人」是运营规则，而这条规则的另一面是：一把钥匙本该只承载**一对**用户的
/// 一两个会话（两台设备各一条连接，落在同一个 Room 里）。所以这里给的是「一台机器重连重叠
/// 也够用」的余量，而不是「能开多少就开多少」——一把流出去的钥匙因此最多占掉几个会话位，
/// 而不是把整档名额吃光。两档共用这一个闸：公益档那把钥匙同样不该能占满 10 组公益名额。
pub const DEFAULT_MAX_SESSIONS_PER_KEY: usize = 4;

/// 限时 TURN 凭据的有效期（`PAIR_TURN_TTL_SECS`，秒；只在配了 `PAIR_TURN_SECRET` 时生效）。
///
/// 24 小时是「一定长过一条会话」的量级：客户端**整条会话只读一次** `iceServers`
/// （`manager.rs` 的 welcome 处理），之后每一轮 ICE 重试都复用同一份凭据，而中继只有在
/// **整条会话重连**时才会重读。凭据短于会话寿命就会在会话中途失效，表现为「打洞突然打不
/// 通了」，所以宁可给长一点：它的作用是让**泄露出去的**那份凭据自己过期，而不是限制正在用
/// 的人。
pub const DEFAULT_TURN_TTL_SECS: u64 = 86_400;

/// 一个 Room 里最多同时有几条「已鉴权、还没走进 `admit`」的连接（`pending`）。
///
/// 正常情况永远只会有 1~2 条：两个人各自握手。给它封顶是因为**不封顶的那条路不需要任何
/// 额度**——同一个 Room 可以被堆上任意多条这样的连接，每条占一个任务、一份 8 KiB 的请求头
/// 缓冲（还在 `HANDSHAKE_TIMEOUT` 那 10 秒里），而且它们的许可都算进容量（`pending`），
/// 于是既拖住自己、也拖住别人。4 条 = 正常用法的两倍余量（握手 + 一次重连重叠）。
pub const MAX_PENDING_PER_ROOM: usize = 4;

/// 握手层每个 IP 允许的**失败**速率（`PAIR_HANDSHAKE_FAILURES_PER_MINUTE`，0 = 不限）。
///
/// 它**按 IP 算、两档都算**：拿错服务器密码、拿错公益密码、公益名额满……每一次被拒都记一笔。
/// 错密码刷 `/ws` 的代价是「一次 SHA256 与一次恒定时间比较」——单次很便宜，但可以无限刷，
/// 而且每次都写一行日志（日志是部署者排障的唯一证据，被刷掉就等于没有）。
///
/// 30 次/分钟对诚实客户端是很宽的门槛：密码不对在客户端是**致命**的（它直接提示改密码、
/// 根本不重试），会按退避重试的是 429 / 503 那一类，而那个退避封顶 30 秒，也就是最多
/// 2 次/分钟——离 30 差十几倍。所以这一项只用来把「一个 IP 的持续爆破」压到 0.5 次/秒。
/// 代价是同一个 NAT 后面的无辜用户会跟着被 429 一段时间（按 IP 记账的固有取舍）。
pub const DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE: f64 = 30.0;

/// 能同时挂着的 TCP 连接数缺省上限（`RelayOptions::max_connections` 的缺省值）。
///
/// 真实部署里这个数由名额推出来（一个会话两条连接，再加一截握手余量，见
/// `server.rs::Config::max_connections`），所以部署者不用管它；这个缺省值是给会话层的单测
/// 与 `RelayOptions::default()` 用的。
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;

/// 预握手（读请求头）阶段的连接数缺省上限（`RelayOptions::max_pre_handshake_connections`
/// 的缺省值）。
///
/// 它与上面那个「真实连接数上限」是**两道不同的闸**：这一道只覆盖「TCP 已经接受、请求头
/// 还没读完」那一段（`HANDSHAKE_TIMEOUT` 之内），放行之后就再也用不到它。它比真实上限宽
/// 得多是有意的——它的职责是「别让一堆半开的连接把任务与内存吃光」，而不是「够不够用」：
/// 把它设成真实额度，等于「随便谁开几条不发请求头的连接就能让所有人拿到 503」。
pub const DEFAULT_MAX_PRE_HANDSHAKE_CONNECTIONS: usize = 2 * DEFAULT_MAX_CONNECTIONS;

/// 预握手阶段**同一个来源地址**最多同时挂几条（0 = 不限）。
///
/// 读请求头时还没有请求头可用，所以这里只能按**对端地址**算：域名模式下那就是前置反代的
/// 地址（所有人共用），direct 模式下就是客户端本身。64 是「同一个 NAT 后面几十个人同时
/// 重连也够」的量级——这一道闸要的是「别让一个来源把预握手池吃光」，不是每 IP 的限额。
pub const DEFAULT_PRE_HANDSHAKE_PER_IP: usize = 64;

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
    /// 空闲太久被回收（**不是**「打洞失败」，见 `DEFAULT_PUBLIC_WINDOW_SECS` /
    /// `DEFAULT_FULL_WINDOW_SECS`）。两档共用这一个码：客户端要做的事完全一样
    /// （重连），而它无从知道对面那一档的窗口是哪一个数。
    pub const IDLE: u16 = 4005;
    /// 这把钥匙的滚动预算用完了（**不是**「你发太快」，见 `DEFAULT_PUBLIC_KEY_BUDGET_BYTES` /
    /// `DEFAULT_FULL_KEY_BUDGET_BYTES`）。它与 `1008`（自己的额度不够）分开，客户端才能说出
    /// 「这台服务器给你的额度用完了」
    /// 而不是一句笼统的格式错误。
    pub const KEY_BUDGET: u16 = 4006;
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
        // 广告出去的是**持续**额度（客户端按它的 2/3 推自己的出站速率）：12 帧/秒与
        // 8 KiB/秒。突发容量（24 帧 / 64 KiB）是中继这边的账，不进 welcome。
        assert_eq!(json["limits"]["framesPerSecond"], 12.0);
        assert_eq!(json["limits"]["bytesPerSecond"], 8.0 * 1024.0);
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

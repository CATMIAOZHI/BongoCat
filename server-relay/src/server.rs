//! HTTP 路由、鉴权与配置。中继的会话逻辑在 `relay.rs`。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tokio::net::{TcpListener, TcpStream};

use crate::auth;
use crate::http::{read_request_head, write_response, write_response_with_headers};
use crate::protocol::{
    is_valid_device_id, is_valid_room_id, Limits, DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE,
    DEFAULT_MAX_BYTES_PER_SECOND, DEFAULT_MAX_CHUNKS_PER_SECOND, DEFAULT_MAX_FRAMES_PER_SECOND,
    DEFAULT_MAX_PUBLIC_PER_IP, DEFAULT_MAX_PUBLIC_SESSIONS, DEFAULT_MAX_SESSIONS,
    DEFAULT_PUBLIC_BURST_BYTES, DEFAULT_PUBLIC_BURST_FRAMES, DEFAULT_PUBLIC_KEY_BUDGET_BYTES,
    DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND, DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND,
    DEFAULT_PUBLIC_WINDOW_SECS, DEFAULT_STALE_AFTER_MS, HEADER_AUTHORIZATION, HEADER_CLIENT,
    HEADER_PROTOCOL, HEADER_ROOM, HEADER_SERVER, HEADER_TIER, HEALTH_PATH,
    MIN_SERVER_PASSWORD_LENGTH, PROTOCOL_VERSION, TIER_HEADER_VALUE, Tier, WEBSOCKET_VERSION,
    WS_PATH,
};
use crate::relay::{self, IpKey, Relay, RelayOptions, RoomRejection, ServerKey};

/// 请求头必须在这个时间内读完：只发一个连接、永远不发请求头的客户端不该占住一个任务
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 一个会话两条连接之外，还允许多少条「正在握手」的连接（见 `Config::max_connections`）。
const HANDSHAKE_HEADROOM: usize = 32;

/// 被准入闸挡下时，最多花多久把请求头读掉（见 `handle` 里那条 503 的注释）。
///
/// 它取得很短是有意的：这一条路上本来就不该有长命任务。
const REJECT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// `Sec-WebSocket-Key` 的合法性（RFC 6455 §4.2.1：16 字节的 base64）
fn is_valid_websocket_key(value: &str) -> bool {
    STANDARD
        .decode(value.trim())
        .is_ok_and(|bytes| bytes.len() == 16)
}

/// 运行期配置。全部可以用环境变量覆盖（见 `load_config`）。
///
/// 多会话（§6）下这里**没有 Pair Secret、也没有 PAIR_AUTH_TOKEN**：一套服务器服务
/// 的是所有自带配对密码的用户，鉴权退化成「同一个 Room 的人拿的 token 摘要一致」。
pub struct Config {
    /// 监督下发给客户端的限流额度，同时就是中继自己的桶容量
    pub limits: Limits,
    /// 同时承载的双人会话数上限（`PAIR_MAX_SESSIONS`）
    pub max_sessions: usize,
    /// 多久没有消息的连接可以被新连接顶替
    pub stale_after: Duration,
    /// `/ws` 的 `server.welcome` 里附带的 ICE 服务器（可选，原样透传）
    pub ice_servers: Option<serde_json::Value>,
    /// 内置 STUN 的 UDP 端口（`PAIR_STUN_PORT`，默认 3479，`0` 关闭）。
    ///
    /// 部署者自己配了 `PAIR_ICE_SERVERS` 时这里是 `None`：以他的配置为准，内置的不启动、
    /// 也不混进广告里（两份 STUN 同时广告只会让客户端多问一次）。
    pub stun_port: Option<u16>,
    /// R36：服务器钥匙表。谁能连上这台服务器、以及进来之后算哪一档，全看它；
    /// 一个变量可以配多把（`;` 分隔），每把的档位由它来自哪个变量决定。
    ///
    /// 配置里**没有**密码原文，也没有它的任何可逆形态（只有 `SHA256(derive_server_token)`
    /// 摘要）——`load_config` 里的局部变量是原文唯一存在过的地方。
    pub server_keys: Vec<ServerKey>,
    /// 公益档（public tier）的额度。它只放行信令，所以比 `limits` 小得多。
    pub public_limits: Limits,
    /// 公益档一条连接的**突发容量**（`PAIR_PUBLIC_BURST_FRAMES` / `PAIR_PUBLIC_BURST_BYTES`）
    pub public_burst_frames: f64,
    pub public_burst_bytes: f64,
    /// 公益档**每把钥匙**的滚动预算（`PAIR_PUBLIC_KEY_BUDGET_BYTES`）。`None` = 不设这一层
    pub public_key_budget: Option<f64>,
    /// 握手层每个 IP 每分钟允许几次失败（`PAIR_HANDSHAKE_FAILURES_PER_MINUTE`）。
    /// `None` = 不限
    pub handshake_failures_per_minute: Option<f64>,
    /// 公益档同时承载的会话数（`PAIR_MAX_PUBLIC_SESSIONS`）。与 `max_sessions` 分开算：
    /// 公益档占不到部署者自己的名额
    pub max_public_sessions: usize,
    /// 同一个 IP 最多几组公益会话（`PAIR_MAX_PUBLIC_PER_IP`）。0 = 不限
    pub max_public_per_ip: usize,
    /// 公益档的空闲回收窗口。`None` = 不回收
    pub public_window: Option<Duration>,
    /// 信不信 `X-Forwarded-For`（`PAIR_TRUST_PROXY`）：只有前面站着可信反代时才打开
    pub trust_proxy: bool,
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_f64(name: &str, default: f64) -> Result<f64, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0 && value.is_finite())
            .ok_or_else(|| format!("{name} 必须是大于 0 的数字，实际是 {text:?}")),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<u64>()
            .map_err(|_| format!("{name} 必须是非负整数，实际是 {text:?}")),
    }
}

/// 布尔开关（`PAIR_TRUST_PROXY`）。只认常见的几种真值，别的写法一律报错——
/// 一个「看着像开了其实没开」的部署，比启动失败更难查。
fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => match text.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("{name} 只能是 1/0（或 true/false），实际是 {text:?}")),
        },
    }
}

/// `PAIR_MAX_SESSIONS` 必须 ≥ 1。
///
/// 0 会让整套服务器一个人都进不来（任何新会话都被判满），那几乎一定是配置事故；
/// 与其默默接受一个「永远 503」的部署，不如启动就报错。
fn env_positive_usize(name: &str, default: usize) -> Result<usize, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|value| *value >= 1)
            .ok_or_else(|| format!("{name} 必须是大于 0 的整数，实际是 {text:?}")),
    }
}

/// 读一个「`0` = 关掉这一层」的数（`PAIR_PUBLIC_KEY_BUDGET_BYTES` /
/// `PAIR_HANDSHAKE_FAILURES_PER_MINUTE`）。
///
/// 单独一个读取器，不塞进 `env_f64`：那一个要求 > 0，因为「每秒 0 帧」等于一个人都发不
/// 出去，几乎一定是配置事故；而这里的 0 有正当含义——关掉这一层限额，与
/// `PAIR_MAX_PUBLIC_PER_IP=0` 同义。负数是真写错了，报错。缺省值因此要求 > 0。
fn env_optional_f64(name: &str, default: f64) -> Result<Option<f64>, String> {
    match env_non_empty(name) {
        None => Ok(Some(default)),
        Some(text) => parse_optional_f64(name, &text),
    }
}

/// [`env_optional_f64`] 的纯函数内核。
///
/// 与 `parse_passwords` 同一个理由抽出来：环境变量在测试里是进程全局的，直接测
/// `load_config` 会互相干扰，所以判定本身要能被单测钉住。
fn parse_optional_f64(name: &str, text: &str) -> Result<Option<f64>, String> {
    // `f64::from_str` 不接受前后空白，而 `.env` 里多打一个空格是很常见的事。走
    // `env_optional_f64` 时 `env_non_empty` 已经削过一次，这一行是给直接调用它的单测
    // 与将来的复用者兜底。错误信息里仍然带上原文，好让人一眼看出自己写的是什么。
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| *value >= 0.0 && value.is_finite())
        .map(|value| (value > 0.0).then_some(value))
        .ok_or_else(|| format!("{name} 必须是非负数字（0 表示关掉这一项限额），实际是 {text:?}"))
}

/// 一个会话两条连接之外，再留多少条握手余量（见 `Config::max_connections`）。
fn derived_max_connections(max_sessions: usize, max_public_sessions: usize) -> usize {
    2 * (max_sessions + max_public_sessions) + HANDSHAKE_HEADROOM
}

/// 读一个「一把或多把密码」的环境变量（`;` 分隔）。
///
/// 同一个变量里的每一把都属于**同一个档位**（完全档看 `PAIR_SERVER_PASSWORD`，公益档看
/// `PAIR_PUBLIC_SERVER_PASSWORD`），所以档位不写在值里：写进去就要用户记语法，还容易和
/// 密码里的字符打架。这样配出来的效果是「每把钥匙各给一个人，换人时只撤销一把」。
///
/// 空白项（多打的分号、行尾分号）忽略；报错时**只说第几把**，绝不回显密码本身
/// ——它是秘密，日志或控制台里出现一次就等于泄露。
fn env_passwords(name: &str, too_short: &str) -> Result<Vec<String>, String> {
    match env_non_empty(name) {
        None => Ok(Vec::new()),
        Some(text) => parse_passwords(name, &text, too_short),
    }
}

/// [`env_passwords`] 的纯函数内核：切分 + 校验。
///
/// 单独抽出来是为了能被单测钉住——环境变量在测试里是进程全局的，直接测 `load_config`
/// 既会互相干扰，也会把别的用例的配置搅乱。
fn parse_passwords(name: &str, text: &str, too_short: &str) -> Result<Vec<String>, String> {
    let mut passwords: Vec<String> = Vec::new();

    for part in text.split(';') {
        let password = part.trim();

        if password.is_empty() {
            continue;
        }

        // 报「第几把」时数的是**密码**，不是分号切出来的槽位：`a;;短` 里那串短的是第 2 把
        // 密码（多打的空分号不该把编号顶偏），跨档重复那条报错也用同一套编号。
        let index = passwords.len() + 1;

        if password.chars().count() < MIN_SERVER_PASSWORD_LENGTH {
            return Err(format!(
                "{name} 的第 {index} 把太短：至少要 {MIN_SERVER_PASSWORD_LENGTH} 个字符（{too_short}）；\
                 多把密码之间用 `;` 分隔",
            ));
        }

        if passwords.iter().any(|existing| existing == password) {
            return Err(format!(
                "{name} 的第 {index} 把和前面某一把完全一样：重复的那把是白写的，删掉一个"
            ));
        }

        passwords.push(password.to_string());
    }

    Ok(passwords)
}

/// 两档的清单里有没有**同一把**钥匙，返回它在公益档清单里第几把（1 起）。
///
/// 同一把钥匙兼两档会让「这次算哪一档」成为二义，也等于把公益那套限制绕过去了（拿公益
/// 密码进来的人会掉回完全档，直接用上中继与 TURN），所以 `load_config` 碰到它就拒绝启动。
/// 单独抽成纯函数是为了能被单测钉住——这条判据在 `load_config` 里，而那个函数读全局
/// 环境变量，没法在测试里安全地调。
fn shared_key(full: &[String], public: &[String]) -> Option<usize> {
    public
        .iter()
        .position(|password| full.iter().any(|key| key == password))
        .map(|index| index + 1)
}

/// 读环境变量组装配置。
pub fn load_config() -> Result<Config, String> {
    let limits = Limits {
        frames_per_second: env_f64("PAIR_MAX_FRAMES_PER_SECOND", DEFAULT_MAX_FRAMES_PER_SECOND)?,
        chunks_per_second: env_f64("PAIR_MAX_CHUNKS_PER_SECOND", DEFAULT_MAX_CHUNKS_PER_SECOND)?,
        bytes_per_second: env_f64("PAIR_MAX_BYTES_PER_SECOND", DEFAULT_MAX_BYTES_PER_SECOND)?,
    };

    let ice_servers = match env_non_empty("PAIR_ICE_SERVERS") {
        None => None,
        Some(text) => Some(
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|error| format!("PAIR_ICE_SERVERS 不是合法 JSON: {error}"))?,
        ),
    };

    let stun_port = match env_non_empty("PAIR_STUN_PORT") {
        None => Some(crate::stun::DEFAULT_STUN_PORT),
        Some(text) => match text.parse::<u16>() {
            Ok(0) => None,
            Ok(port) => Some(port),
            Err(_) => {
                return Err(format!(
                    "PAIR_STUN_PORT 必须是 0~65535 的整数（0 表示关闭内置 STUN），实际是 {text:?}"
                ))
            }
        },
    }
    .filter(|_| ice_servers.is_none());

    // R36：服务器密码是**必填**的。它是「谁能用这台服务器」的唯一门槛：没有它，
    // 任何人只要知道地址就能开一个自己的会话（还会顺走 welcome 里的 TURN 凭据）。
    // 与其允许一个默认开放、随时可能被白嫖的部署，不如启动就报错说清楚怎么设。
    // 这里可以写多把（`;` 分隔）：每把都是完全档，给出去一把不影响别的。
    let server_passwords = env_passwords(
        "PAIR_SERVER_PASSWORD",
        "太短的门槛挡不住爆破，也挡不住猜",
    )?;

    if server_passwords.is_empty() {
        return Err(format!(
            "缺少 PAIR_SERVER_PASSWORD：请在 .env 里设置一个至少 {MIN_SERVER_PASSWORD_LENGTH} \
             字符的服务器密码（可以用 `cargo run --bin generate-pair -- --server` 生成），\
             填完再重启；客户端要用同一个值填「服务器密码」。要发给多个人，可以写多把，\
             用 `;` 分隔（每把各给一个人，换人时只撤销一把）"
        ));
    }

    // 公益档（可选）。设了它，拿到这个密码的人就借这台服务器打洞：只转发信令、只广告
    // STUN、占自己的名额，占不到部署者那一档的任何东西。
    // 同样可以写多把（`;` 分隔），每把都是公益档。
    let public_passwords = env_passwords(
        "PAIR_PUBLIC_SERVER_PASSWORD",
        "公益密码是**公开**的，长度是唯一的在线爆破阻力",
    )?;

    // 同一把钥匙不能同时属于两档：「这次算哪一档」会成为二义，而且等于把公益那套限制
    // 绕过去了（拿公益密码进来的人会掉回完全档，直接用上中继与 TURN）。
    if let Some(index) = shared_key(&server_passwords, &public_passwords) {
        return Err(format!(
            "PAIR_PUBLIC_SERVER_PASSWORD 的第 {index} 把和 PAIR_SERVER_PASSWORD 里的一把\
             相同：公益档与你自己那一档必须是两把不同的钥匙"
        ));
    }

    let server_keys: Vec<ServerKey> = server_passwords
        .iter()
        .map(|password| ServerKey::new(Tier::Full, password))
        .chain(
            public_passwords
                .iter()
                .map(|password| ServerKey::new(Tier::Public, password)),
        )
        .collect();

    // 这里**不用** `env_positive_usize`：公益档的 0 有正当含义（关掉这一档，但把密码留着
    // 免得以后又要重新分发），不像 `PAIR_MAX_SESSIONS` 那样一定是配置事故——0 会让它
    // 整台服务器一个人都进不来。所以 0 照收，只在下面打一条提醒。
    let max_public_sessions = env_u64(
        "PAIR_MAX_PUBLIC_SESSIONS",
        DEFAULT_MAX_PUBLIC_SESSIONS as u64,
    )? as usize;

    // 0 只打警告、不报错：部署者可能只是暂时关掉公益档，没道理把整套服务器拖死
    if !public_passwords.is_empty() && max_public_sessions == 0 {
        eprintln!(
            "提醒：PAIR_PUBLIC_SERVER_PASSWORD 已设置，但 PAIR_MAX_PUBLIC_SESSIONS=0，\
             公益档实际上一个人都进不来"
        );
    }

    let public_window_secs = env_u64("PAIR_PUBLIC_WINDOW_SECS", DEFAULT_PUBLIC_WINDOW_SECS)?;

    // 公益档**每把钥匙**的滚动预算（0 = 不设这一层）。记账挂钥匙而不是挂连接 / 房间 /
    // IP 的理由见 `protocol.rs` 里那条常量的注释。
    let public_key_budget =
        env_optional_f64("PAIR_PUBLIC_KEY_BUDGET_BYTES", DEFAULT_PUBLIC_KEY_BUDGET_BYTES)?;

    // 握手层每个 IP 的失败速率。它和公益档无关，是**部署者自己那一档**的护栏：拿错密码
    // 刷 `/ws` 的代价本来只是一次摘要比较，真正会被打爆的是日志（排障的唯一证据）。
    let handshake_failures_per_minute = env_optional_f64(
        "PAIR_HANDSHAKE_FAILURES_PER_MINUTE",
        DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE,
    )?;

    let max_sessions = env_positive_usize("PAIR_MAX_SESSIONS", DEFAULT_MAX_SESSIONS)?;

    Ok(Config {
        limits,
        max_sessions,
        stale_after: Duration::from_millis(env_u64("PAIR_STALE_AFTER_MS", DEFAULT_STALE_AFTER_MS)?),
        ice_servers,
        stun_port,
        server_keys,
        public_limits: Limits {
            frames_per_second: env_f64(
                "PAIR_PUBLIC_MAX_FRAMES_PER_SECOND",
                DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND,
            )?,
            // 公益档一个分片都不转发（帧白名单只有 kind 8），这一维只是 `Limits` 的形状
            // 要求：沿用部署者那一档的分片值，免得广告一个「被当成没广告」的 0
            chunks_per_second: limits.chunks_per_second,
            bytes_per_second: env_f64(
                "PAIR_PUBLIC_MAX_BYTES_PER_SECOND",
                DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND,
            )?,
        },
        public_burst_frames: env_f64("PAIR_PUBLIC_BURST_FRAMES", DEFAULT_PUBLIC_BURST_FRAMES)?,
        public_burst_bytes: env_f64("PAIR_PUBLIC_BURST_BYTES", DEFAULT_PUBLIC_BURST_BYTES)?,
        public_key_budget,
        handshake_failures_per_minute,
        max_public_sessions,
        max_public_per_ip: env_u64("PAIR_MAX_PUBLIC_PER_IP", DEFAULT_MAX_PUBLIC_PER_IP as u64)?
            as usize,
        public_window: (public_window_secs > 0)
            .then(|| Duration::from_secs(public_window_secs)),
        trust_proxy: env_bool("PAIR_TRUST_PROXY", false)?,
    })
}

pub async fn listen_address() -> Result<String, String> {
    Ok(env_non_empty("PAIR_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_string()))
}

impl Config {
    /// 把配置折进会话层。
    ///
    /// 单独抽出来是为了让「配置项 → 会话层字段」只映射一次：`main.rs` 与集成测试都调它，
    /// 以后再加一项也不会出现「主程序填了、测试没填」那种分叉。
    pub fn relay_options(&self, stun_port: Option<u16>) -> RelayOptions {
        RelayOptions {
            limits: self.limits,
            public_limits: self.public_limits,
            public_burst_frames: self.public_burst_frames,
            public_burst_bytes: self.public_burst_bytes,
            public_key_budget: self.public_key_budget,
            handshake_failures_per_minute: self.handshake_failures_per_minute,
            max_sessions: self.max_sessions,
            // 准入闸不单独配一项：一个会话两条连接，再加一截握手余量。放在这里（而不是
            // `load_config`）是为了让它**跟着名额走**——`max_public_sessions` 在配置阶段
            // 还可能被改动，写死一次就可能小于「按配置本来就该跑得起来的量」。
            max_connections: derived_max_connections(
                self.max_sessions,
                self.max_public_sessions,
            ),
            max_public_sessions: self.max_public_sessions,
            max_public_per_ip: self.max_public_per_ip,
            public_window: self.public_window,
            stale_after: self.stale_after,
            ice_servers: self.ice_servers.clone(),
            stun_port,
            server_keys: self.server_keys.clone(),
            trust_proxy: self.trust_proxy,
        }
    }
}

/// 接受连接，直到监听器出错。
///
/// 只收 `relay`：`Config` 里的每一项都已经被折进 `Relay` 了（限流、容量、陈旧判定、
/// ICE），会话层之外没有第二份真相。
pub async fn serve(listener: TcpListener, relay: Arc<Relay>) {
    loop {
        let accepted = listener.accept().await;

        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("接受连接失败：{error}");

                continue;
            }
        };

        // 实时帧很小，禁用 Nagle 才不会让状态卡在缓冲里
        let _ = stream.set_nodelay(true);

        let relay = Arc::clone(&relay);

        tokio::spawn(async move {
            if let Err(error) = handle(stream, peer, relay).await {
                // 客户端断线是常态：记一行即可，不影响其它连接
                eprintln!("连接 {peer} 结束：{error}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, peer: SocketAddr, relay: Arc<Relay>) -> Result<(), String> {
    // 准入闸（`Config::max_connections`）。没有它的话「开任意多条 TCP」是不花钱的：
    // `/health` 与握手都不过闸，每条连接占一个任务加一份 8 KiB 的请求头缓冲，直到把
    // 宿主机拖垮——那时部署者自己那一档也一起连不上。
    //
    // 满了就当场回 503 并断开，**不排队等**：排队等于把已经接受的 socket 堆在这里，
    // 那正是这道闸要防的东西。这里用的是**对端地址**（不是 `X-Forwarded-For` 算出来的
    // 那个），也不记进每 IP 的失败额度：请求头还没读，域名模式下对端是反代容器，按它
    // 记账会把所有人算成同一个 IP。所以只留一行、而且每 10 秒最多一行。
    let _permit = match relay.try_connection_permit() {
        Some(permit) => permit,
        None => {
            // 先把请求头**尽量**读掉（上限 `REJECT_DRAIN_TIMEOUT`）：不读就关的话，客户端
            // 已经发出来的请求还躺在接收缓冲里，内核会直接回 RST——那条 503 还没被读到就被
            // 丢掉，客户端只看到「连接被重置」，而这正是「明明服务器活着、却看不出为什么
            // 连不上」的来源。真实客户端都是连上就把请求发出来的，所以这一步瞬间完成；
            // 上限取得很短，所以卡在这里慢慢发头的人也只能多占这个任务这么久。
            let _ =
                tokio::time::timeout(REJECT_DRAIN_TIMEOUT, read_request_head(&mut stream)).await;

            if relay.note_connection_limit().await {
                eprintln!("连接 {peer} 被拒绝：连接数已达上限（HTTP 503）");
            }

            return write_response(
                &mut stream,
                503,
                "Service Unavailable",
                "text/plain; charset=utf-8",
                "server is at its connection limit",
            )
            .await
            .map_err(|error| error.to_string());
        }
    };

    // 握手超时：只连不发（或慢慢发）的客户端不能无限占着一个任务
    let head = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_request_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => return Err("握手超时".into()),
    };

    // 公益档的每 IP 限额、以及下面那条握手限速都按哪个 IP 算。Caddy 那一跳只在内网，所以
    // 域名模式下要靠 `X-Forwarded-For`（`PAIR_TRUST_PROXY=1`，compose 已默认打开）；
    // direct 模式对端就是客户端本身，不需要它。
    //
    // 取的是**这个头的每一行**，不是第一行：它可能被反代拆成多行（见 `RequestHead::header_values`）。
    let forwarded_for = head.header_values("x-forwarded-for");
    let client = relay::client_ip(peer, &forwarded_for, relay.trust_proxy());

    // 每 IP 的握手失败限速（`PAIR_HANDSHAKE_FAILURES_PER_MINUTE`）：一个 IP 连续把密码打错
    // （或者反复发格式不对的请求）时，这里直接回 429，不再往下走——省下鉴权的摘要计算，
    // 也省下继续刷日志。诚实客户端碰不到它：密码不对在客户端是**致命**的（它直接提示改
    // 密码、根本不重试），会按退避重试的是 429 / 503 那一类，而那个退避封顶 30 秒，也就是
    // 最多 2 次/分钟——离 30 差十几倍。
    if !relay.handshake_allowed(client).await {
        reject(&relay, peer, client, "这个地址的失败次数太多", 429).await;

        return write_response(
            &mut stream,
            429,
            "Too Many Requests",
            "text/plain; charset=utf-8",
            "too many failed handshakes",
        )
        .await
        .map_err(|error| error.to_string());
    }

    if head.path() == HEALTH_PATH {
        // §28：只说「我活着、协议是 1、需要服务器密码」。**不暴露**任何 Room、deviceId
        // 或密钥信息；`passwordRequired` 是常量，用来让部署者一条 curl 就确认自己装对了
        // `publicTier` 同理（有没有配公益密码）。`serverKeys` 只报**把数**（完全档 / 公益档
        // 各配了几把），不带任何能拿去试的东西——部署者配多把时靠它确认自己没写漏。
        let body = format!(
            "{{\"ok\":true,\"protocol\":{PROTOCOL_VERSION},\"mode\":\"multi-pair\",\
             \"passwordRequired\":true,\"publicTier\":{},\
             \"serverKeys\":{{\"full\":{},\"public\":{}}}}}",
            relay.has_public_tier(),
            relay.server_key_count(Tier::Full),
            relay.server_key_count(Tier::Public)
        );

        return write_response(&mut stream, 200, "OK", "application/json", &body)
            .await
            .map_err(|error| error.to_string());
    }

    if head.path() != WS_PATH {
        // 这一路**不记日志**：公网扫描器会把它刷满，而它跟凭据无关——客户端自己
        // 会看到 404 与「服务器地址路径不对」，不需要服务器这边也留痕
        return write_response(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "not found",
        )
        .await
        .map_err(|error| error.to_string());
    }

    if !head
        .header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        return write_response(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            "expected websocket upgrade",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // 版本不对时按 RFC 6455 §4.4 回 426 并带上支持的版本
    if head.header("sec-websocket-version") != Some(WEBSOCKET_VERSION) {
        return write_response_with_headers(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            &format!("Sec-WebSocket-Version: {WEBSOCKET_VERSION}\r\n"),
            "unsupported websocket version",
        )
        .await
        .map_err(|error| error.to_string());
    }

    if !head
        .header("sec-websocket-key")
        .is_some_and(is_valid_websocket_key)
    {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid websocket key",
        )
        .await
        .map_err(|error| error.to_string());
    }

    let expected_protocol = PROTOCOL_VERSION.to_string();

    if head.header(HEADER_PROTOCOL) != Some(expected_protocol.as_str()) {
        reject(&relay, peer, client, "协议版本不支持", 426).await;

        return write_response(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            "unsupported protocol",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // R36：**服务器密码排在最前面**。它是「谁能用这台服务器」的门槛，与「哪一对用户」
    // 完全无关：没有它的人不该能建会话、不该能探测 Room 是否存在、更不该拿到
    // `server.welcome` 里的 TURN 凭据（那是按流量计费的东西）。
    //
    // 用 403 而不是 401：401 在这套协议里已经表示「配对密码不对」（Room verifier 不匹配），
    // 客户端要把两者显示成不同的话。Cloudflare 版不会返回 403，所以这个取值不会撞车。
    let server_token = head
        .header(HEADER_SERVER)
        .unwrap_or_default()
        .trim()
        .to_string();

    if server_token.is_empty() {
        reject(&relay, peer, client, "缺少服务器密码", 403).await;

        return write_response(
            &mut stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            "server password required",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // 拿到的除了档位还有**第几把**钥匙：公益档的滚动预算记在钥匙上（见
    // `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`），所以序号要一路带到 `reserve` / `allow`。
    let Some((tier, key_index)) = relay.classify_server_token(&server_token) else {
        reject(&relay, peer, client, "服务器密码不正确", 403).await;

        return write_response(
            &mut stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            "server password incorrect",
        )
        .await
        .map_err(|error| error.to_string());
    };

    // 公益档要求客户端**认得这一档**（`X-Bongo-Tier`）。它换来一条明确的兼容边界：
    // 老客户端拿着公益密码会在这里拿到 426（「你这版客户端还不认公益档，请升级」），
    // 而不是「连上之后被踢」或者「界面显示已连接、其实什么都通不了」。
    //
    // 用 426 而不是 403：客户端已经把 426 显示成「两边版本不一致：请把它们都升级到
    // 最新版」，那正是这种情况该说的话；而 403 会被显示成「服务器密码不对」，把人
    // 引向完全错误的方向。
    if tier == Tier::Public
        && head.header(HEADER_TIER) != Some(TIER_HEADER_VALUE)
    {
        reject(&relay, peer, client, "公益档需要新客户端", 426).await;

        return write_response(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            "public tier requires a tier aware client",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §4：中继要靠 Room 才能找到「这次连接属于哪个会话」，所以它排在鉴权前面。
    // 格式规则是公开的（43 个 base64url 字符），不是秘密，400 这里不泄露任何东西。
    let room_id = head
        .header(HEADER_ROOM)
        .unwrap_or_default()
        .trim()
        .to_string();

    if !is_valid_room_id(&room_id) {
        reject(&relay, peer, client, "会话标识不合法", 400).await;

        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid room id",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // 空的 Authorization 连摘要都算不出来：先按未鉴权拒掉
    let token = auth::bearer_token(head.header(HEADER_AUTHORIZATION));

    if token.is_empty() {
        reject(&relay, peer, client, "缺少配对密码", 401).await;

        return write_response(
            &mut stream,
            401,
            "Unauthorized",
            "text/plain; charset=utf-8",
            "authentication failed",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §8 / §9 / §27：容量与密钥判定都在升级之前完成，客户端才能按状态码区分
    // 「配对密码不正确」（401）与「服务器会话已满」（503）。
    let reservation = match relay
        .reserve(&room_id, auth::auth_verifier(&token), tier, key_index, client)
        .await
    {
        Ok(reservation) => reservation,
        Err(RoomRejection::AuthMismatch) => {
            reject(&relay, peer, client, "配对密码与这个会话不一致", 401).await;

            return write_response(
                &mut stream,
                401,
                "Unauthorized",
                "text/plain; charset=utf-8",
                "authentication failed",
            )
            .await
            .map_err(|error| error.to_string());
        }
        Err(RoomRejection::Capacity) => {
            reject(&relay, peer, client, "服务器会话已满", 503).await;

            return write_response(
                &mut stream,
                503,
                "Service Unavailable",
                "text/plain; charset=utf-8",
                "server capacity reached",
            )
            .await
            .map_err(|error| error.to_string());
        }
        Err(RoomRejection::TierMismatch) => {
            reject(&relay, peer, client, "会话档位与这次凭据不一致", 409).await;

            return write_response(
                &mut stream,
                409,
                "Conflict",
                "text/plain; charset=utf-8",
                "server tier mismatch",
            )
            .await
            .map_err(|error| error.to_string());
        }
        Err(RoomRejection::PublicIpLimit) => {
            reject(&relay, peer, client, "这个 IP 的公益会话已满", 429).await;

            return write_response(
                &mut stream,
                429,
                "Too Many Requests",
                "text/plain; charset=utf-8",
                "too many public sessions from this address",
            )
            .await
            .map_err(|error| error.to_string());
        }
        // 同一个 Room 上「已放行、还没走进 `admit`」的连接堆得太多（见 `MAX_PENDING_PER_ROOM`）。
        // 正常用法的峰值是 2（两个人握手），所以走到这里要么是这个会话正在被刷，要么是它的
        // 客户端一直在重连而对面接不上——两种情况都该让对方等一下再试，而不是继续堆。
        Err(RoomRejection::TooManyPending) => {
            reject(&relay, peer, client, "这个会话正在握手的连接太多", 429).await;

            return write_response(
                &mut stream,
                429,
                "Too Many Requests",
                "text/plain; charset=utf-8",
                "too many pending handshakes for this room",
            )
            .await
            .map_err(|error| error.to_string());
        }
    };

    // 先通过房间校验再看 deviceId：未通过鉴权的请求不该能探测 deviceId 的合法性
    // （新建会话的情况没有共享凭据可验，这里只能保证「已有会话」不被探测）。
    // 归一成小写：同一个 UUID 用大写重连时不能被当成第三个人而自锁
    let device_id = head
        .header(HEADER_CLIENT)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if !is_valid_device_id(&device_id) {
        // 名额已经占上了，这里必须还回去，否则一个拼错 deviceId 的客户端会永久占住一个会话位
        relay.release(reservation).await;
        reject(&relay, peer, client, "设备标识不合法", 400).await;

        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid client id",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §29：日志只写 Room 指纹，不写完整 ROOM_ID
    println!(
        "{peer} 已通过鉴权（device {device_id}，room {}）",
        auth::room_fingerprint(&room_id)
    );

    relay.serve(stream, head, device_id, reservation).await
}

/// 被拒绝的连接要留下**一行**不含秘密的痕迹（P2-2），而且这一行本身要被限频。
///
/// 此前只有「通过鉴权」会打日志，于是「两台设备连不上、`docker compose logs` 一片空白」
/// 时分不清是「客户端根本没连到这台服务器」还是「被 401/503 挡在门外」。这里只写对端
/// 地址、阶段与状态码——不写 token、不写 ROOM_ID、不写密码。
///
/// 限频是另一件事：拿错密码刷 `/ws` 的代价本来只是一次摘要比较，能被打爆的是**日志**
/// ——它是部署者排障的唯一证据，被刷掉就等于没有。记多少、什么时候静默由
/// `Relay::note_handshake_failure` 一个桶说了算（同一个桶也决定这个 IP 会不会被 429），
/// 所以「拒绝了但没写日志」不是丢证据：那说明前后一分钟里已经有 30 行同样的东西了。
async fn reject(relay: &Relay, peer: SocketAddr, ip: IpKey, reason: &str, status: u16) {
    if relay.note_handshake_failure(ip).await {
        eprintln!("连接 {peer} 被拒绝：{reason}（HTTP {status}）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "PAIR_SERVER_PASSWORD";
    const WHY: &str = "测试用";

    /// 一把密码就是从前那样：行为必须完全不变
    #[test]
    fn a_single_password_stays_one_key() {
        assert_eq!(
            parse_passwords(NAME, "  my-own-long-password  ", WHY).unwrap(),
            vec!["my-own-long-password".to_string()]
        );
    }

    /// 多把：`;` 分隔，空白项忽略（多打的分号、行尾分号不该让人以为配坏了）
    #[test]
    fn several_passwords_are_split_and_trimmed() {
        assert_eq!(
            parse_passwords(NAME, "password-for-alice; password-for-bob ;;", WHY).unwrap(),
            vec![
                "password-for-alice".to_string(),
                "password-for-bob".to_string()
            ]
        );
    }

    /// 报错要说清第几把，而且不能把密码回显出来
    #[test]
    fn a_short_or_repeated_password_is_rejected_by_index() {
        let short = parse_passwords(NAME, "long-enough-password;short", WHY).unwrap_err();

        assert!(short.contains("第 2 把"), "{short}");
        assert!(!short.contains("short"), "报错里不该出现密码：{short}");

        let repeated =
            parse_passwords(NAME, "password-for-alice;password-for-alice", WHY).unwrap_err();

        assert!(repeated.contains("第 2 把"), "{repeated}");
        assert!(!repeated.contains("password-for-alice"), "{repeated}");
    }

    /// 编号数的是**密码**，不是分号切出来的槽位：多打的空分号不该把「第几把」顶偏
    /// （跨档重复那条报错也用同一套编号，两处读起来要是同一件事）
    #[test]
    fn empty_slots_do_not_shift_the_index() {
        let short = parse_passwords(NAME, ";;long-enough-password;short", WHY).unwrap_err();

        assert!(short.contains("第 2 把"), "{short}");
    }

    /// 同一把钥匙不能兼两档：两档清单里出现同一串时，要说清是公益档里第几把
    #[test]
    fn a_key_may_not_sit_in_both_tiers() {
        let full = vec!["password-for-alice".to_string(), "password-for-bob".to_string()];
        let public = vec![
            "volunteer-1-password".to_string(),
            "password-for-bob".to_string(),
        ];

        assert_eq!(shared_key(&full, &public), Some(2));
        // 两档各管各的（同一档内部重复由 `parse_passwords` 挡，跨档只看交集）
        assert_eq!(
            shared_key(
                &full,
                &["volunteer-1-password".to_string(), "volunteer-2-password".to_string()]
            ),
            None
        );
        assert_eq!(shared_key(&[], &public), None);
        assert_eq!(shared_key(&full, &[]), None);
    }

    /// 空值 / 只有分号 = 一把都没有（调用方据此判「压根没设过」）
    #[test]
    fn an_empty_value_yields_no_keys() {
        assert!(parse_passwords(NAME, "   ", WHY).unwrap().is_empty());
        assert!(parse_passwords(NAME, " ; ; ", WHY).unwrap().is_empty());
    }

    /// 「`0` = 关掉这一层」的读数：正数照收，`0` 是「不设这一层」而不是「一个字节都不许发」
    #[test]
    fn an_optional_limit_reads_zero_as_off() {
        let name = "PAIR_PUBLIC_KEY_BUDGET_BYTES";

        assert_eq!(parse_optional_f64(name, "16777216").unwrap(), Some(16777216.0));
        assert_eq!(parse_optional_f64(name, " 0 ").unwrap(), None);
        assert_eq!(parse_optional_f64(name, "0.0").unwrap(), None);
    }

    /// 写错的数字要报错、而且要说清是哪个变量：一个「当成没设」的负值会让部署者以为
    /// 自己设的那一层限额生效了
    #[test]
    fn an_optional_limit_rejects_what_is_not_a_number() {
        let name = "PAIR_HANDSHAKE_FAILURES_PER_MINUTE";

        assert!(parse_optional_f64(name, "-1").is_err());
        assert!(parse_optional_f64(name, "十").is_err());
        assert!(parse_optional_f64(name, "inf").is_err());
        assert!(parse_optional_f64(name, "nan").is_err());

        let error = parse_optional_f64(name, "abc").unwrap_err();

        assert!(error.contains(name), "{error}");
    }

    /// 连接数上限由名额推出来：一个会话两条连接，再加一截握手余量。
    ///
    /// 它**不可能**小于「按配置本来就该跑得起来的量」——这正是「不给部署者加一项配置」的
    /// 前提：一个小于名额的连接上限会把服务器变成「名额还有、就是连不上」。
    #[test]
    fn the_connection_cap_follows_the_capacity() {
        // 40 组完全档 + 10 组公益档 = 100 条连接，再加 32 条握手余量
        assert_eq!(derived_max_connections(40, 10), 132);
        // 两档都关掉大半时也留得住最基本的握手并发
        assert_eq!(derived_max_connections(1, 0), HANDSHAKE_HEADROOM + 2);
    }
}

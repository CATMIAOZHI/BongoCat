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
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, OwnedSemaphorePermit, Semaphore};
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
    self, close_code, is_known_frame_kind, Limits, ServerFrame, Tier,
    DEFAULT_FULL_KEY_BUDGET_BYTES, DEFAULT_FULL_WINDOW_SECS, DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE,
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_PRE_HANDSHAKE_CONNECTIONS, DEFAULT_MAX_SESSIONS_PER_KEY,
    DEFAULT_PRE_HANDSHAKE_PER_IP, DEFAULT_PUBLIC_BURST_BYTES, DEFAULT_PUBLIC_BURST_FRAMES,
    DEFAULT_PUBLIC_KEY_BUDGET_BYTES, FRAME_HEADER_SIZE, FRAME_KIND_SIGNAL,
    FRAME_KIND_TRANSFER_CHUNK, LAST_SEEN_WRITE_INTERVAL_MS, MAX_BINARY_FRAME_SIZE,
    MAX_PENDING_PER_ROOM, MAX_PUBLIC_FRAME_SIZE, PAIR_SIZE, PUBLIC_WS_MESSAGE_SIZE,
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
    /// 创建这个会话的那条连接用的是**第几把**服务器钥匙（`sweep` 那行日志用它回答
    /// 「是哪把钥匙在用」）。
    ///
    /// 同一个会话里的两个人可以拿不同的钥匙（只要档位一样），所以它记的是**先到者**的
    /// 那一把——它是「这个会话是谁带来的」这个方向上的线索，不是精确归属。中继看不到
    /// 载荷（信令是端到端加密的），这是部署者事后唯一能看出「哪把钥匙在夹带」的东西。
    key_index: usize,
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
    /// 这个会话累计**真的转发出去**多少帧 / 多少字节（`sweep` 那行日志用）。
    ///
    /// 「真的转发出去」= 那一刻房间里有对端可收：发给空气的帧不算，否则这份统计与部署者
    /// 在出口看到（或没看到）的流量对不上，作为「谁在夹带」的证据就失效了。
    forwarded_frames: u64,
    forwarded_bytes: u64,
}

/// 每个连接的出站队列容量。
///
/// 队列是**有界**的，而且转发时用 `send().await` 等空位：对端 TCP 停摆（手机进
/// 隧道、笔记本睡眠）时，中继会停止读发送方，反压自然传回发送方的 socket，
/// 而不是在这里把内存吃光。真被拖死（见 `FORWARD_TIMEOUT`）才摘掉那个对端。
const OUTBOUND_QUEUE: usize = 32;

/// 公益档连接的出站队列容量。
///
/// 它的帧上限是 64 KiB，所以 32 条队列本身就是 2 MiB——而它的诚实流量是「一轮十来帧、
/// 之后静默」，8 条（512 KiB）已经远超任何真实需要。加上写缓冲 256 KiB 与读上限
/// 128 KiB，公益档的单连接最坏内存是 **0.875 MiB**（完全档约 44 MiB）。
const PUBLIC_OUTBOUND_QUEUE: usize = 8;

/// 公益档连接的写缓冲上限（完全档是 4 MiB）。
///
/// 真正的流控靠上面那个有界队列，这里只是「对端停摆时不无限攒」的兜底；公益档的帧
/// 本来就小，256 KiB 足够。
const PUBLIC_WS_WRITE_BUFFER: usize = 256 * 1024;

/// 公益档每把钥匙的预算按多长时间回填（`DEFAULT_PUBLIC_KEY_BUDGET_BYTES` 说的是「每小时」）。
const KEY_BUDGET_WINDOW_SECS: f64 = 3600.0;

/// 一帧最多能等多久（见 `Relay::allow`、`WaitBudget`）。
///
/// 超过它就还是照旧断开（`4006`）：那说明额度配得比回填速度还紧，等下去只会把连接挂死。
/// 这个数其实由**客户端**定，不是由我们定：等待期间我们**不读**这条连接、靠 TCP 背压当
/// 限速，而客户端每一次 `sink.send` 外面套着 `SEND_TIMEOUT`（10 秒，见 `pair/manager.rs`）
/// ——等过 10 秒，客户端看到的是「发送超时」并把整条会话重启，那比一句说得清的 `4006` 差
/// 得多（原因看不见，用户只会看到反复重连）。5 秒给「帧本身还要传一会儿」留了余量，同时
/// 远小于中继自己的空闲回收（公益档 180 秒 / 完全档 300 秒，都是 4005）与客户端等中继
/// Pong 的窗口（`2 × 心跳` = 120 秒）。
///
/// 它管的是**整帧**而不是单轮：同一把钥匙上还有别的会话在抢额度时，每一轮等待都合法、却
/// 能一直睡下去，所以累计等待由 `WaitBudget` 扣这份预算（见 `serve` 的读循环；扣的是**实际
/// 睡的那个数**，含下面那个下限）。
const KEY_BUDGET_WAIT_LIMIT: Duration = Duration::from_secs(5);

/// 等待的最小步长：差几纳秒时 `Duration::from_secs_f64` 会截断成 0，不兜一下就是空转。
///
/// 它同时会被算进 [`WaitBudget`] 的账（见那里的注释）：预算钉的是墙钟上界，所以每轮实地
/// 睡出去多少就扣多少，不能只扣「报出来的等待」。
const KEY_BUDGET_WAIT_FLOOR: Duration = Duration::from_millis(1);

/// [`Bucket::wait_for`] 能报出来的时长上限。
///
/// 只为挡住天文数字（`Duration::from_secs_f64` 在超出 `u64::MAX` 秒时会 panic），所以它
/// **必须**大于 `KEY_BUDGET_WAIT_LIMIT`：不然「要等很久」会被夹到恰好等于等待上限，
/// 于是每一轮都等到上限、又还是装不下，变成永远等下去。
const KEY_BUDGET_WAIT_REPORT_CAP_SECS: f64 = 86_400.0;

/// 「连接数已达上限」那行日志的抑制窗口。
///
/// 走到那一步说明服务器已经满负荷，而满负荷时连接尝试只会更多——不抑制的话这行会自己把
/// 日志刷满，反而把「为什么满了」的线索盖掉。10 秒一行足够知道正在发生什么。
const CONNECTION_LIMIT_NOTICE_INTERVAL: Duration = Duration::from_secs(10);

/// `handshake_failures` 这张表到多少项之后，**再插一个新地址**时顺手清一遍
/// （见 `Relay::note_handshake_failure`）。
const HANDSHAKE_FAILURE_TABLE_SWEEP: usize = 1024;

/// 转发时最多等一个对端消费多久；超时说明这条连接已经停摆，直接摘掉
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

/// 连接结束时最多等 writer 把队列里剩下的帧（例如那条关闭帧）发完多久。
///
/// 正常情况是毫秒级；只有对端 socket 停摆时才会等满——那时排队的帧本来就发不出去，
/// 直接中止 writer 让 socket 真正关闭。
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 令牌桶的**形状**：容量（一次能突发多少）与回填速率（长期能持续多快）是**两个数**。
///
/// 用令牌桶而不是固定窗口，是因为固定窗口在边界会允许双倍突发，而 R4 要求
/// 「状态变化立即发送」，一次抖动就可能被误判成超限并关掉连接。
///
/// 只用一个数（容量 = 速率）在完全档上是对的——那正是 Cloudflare 版的形状，`Limits`
/// 自己就是它的语义。但公益档不能这么算：它的诚实流量是「一轮几 KB 的突发 + 长期静默」，
/// 用一个数就只能二选一——小到装不下一轮打洞，或者大到等于没有上限（「256 KiB/秒」
/// 实际是「256 KiB 的突发」，一小时能灌 900 MB）。所以这里把两个维度分开。
#[derive(Debug, Clone, Copy)]
struct Quota {
    frames_cap: f64,
    frames_rate: f64,
    chunks_cap: f64,
    chunks_rate: f64,
    bytes_cap: f64,
    bytes_rate: f64,
}

/// 「这一维不管」用的数：取一个大而**有限**的值。
///
/// 不能用 `f64::INFINITY`。理由不是「`inf - inf` 是 `NaN` 会关掉连接」——那一条现在恰好
/// 不会发生：`take` 只减有限值，而 `refill` 里 `elapsed * inf` 虽然在同一时刻连扣两笔时
/// 会算出 `0.0 * inf = NaN`，`f64::min` 的语义又正好是「一个是 NaN 就返回另一个」，于是桶
/// 值仍然是 `inf`、`inf - 1.0 >= 0.0` 成立。问题在于这条正确性**挂在一处很容易被改掉的
/// 细节上**（把这个 `min` 换成 `f64::maximum` 或者手写 `if`，`NaN` 就会留在桶里，之后每一次
/// 扣减都失败）。取一个有限的大数就没有这个隐含前提。
const NEVER_BINDS: f64 = 1e12;

impl Quota {
    /// 容量 = 速率。完全档用它，行为与 CF 版逐条一致。
    fn steady(limits: Limits) -> Self {
        Self {
            frames_cap: limits.frames_per_second,
            frames_rate: limits.frames_per_second,
            chunks_cap: limits.chunks_per_second,
            chunks_rate: limits.chunks_per_second,
            bytes_cap: limits.bytes_per_second,
            bytes_rate: limits.bytes_per_second,
        }
    }

    /// 公益档的一条连接：突发给足（一轮打洞一口气发得完），持续压到涓流。
    fn public_connection(limits: Limits, burst_frames: f64, burst_bytes: f64) -> Self {
        Self {
            frames_cap: burst_frames,
            frames_rate: limits.frames_per_second,
            // 公益档一个分片都不转发（帧白名单只有 kind 8），分片那一维只是个形状
            chunks_cap: limits.chunks_per_second,
            chunks_rate: limits.chunks_per_second,
            bytes_cap: burst_bytes,
            bytes_rate: limits.bytes_per_second,
        }
    }

    /// 只按**帧数**算的闸（每 IP 的握手失败限速用它）：字节与分片两维给一个不会拦住的数。
    fn frames_only(cap: f64, rate: f64) -> Self {
        Self {
            frames_cap: cap,
            frames_rate: rate,
            chunks_cap: NEVER_BINDS,
            chunks_rate: NEVER_BINDS,
            bytes_cap: NEVER_BINDS,
            bytes_rate: NEVER_BINDS,
        }
    }

    /// 只按**字节**算的滚动预算（每把公益钥匙的预算用它）。帧与分片两维不拦：预算要管的
    /// 是「传了多少数据」，而一帧一帧地数会让「用大帧夹带」比「用小帧夹带」更划算。
    fn bytes_only(cap: f64, rate: f64) -> Self {
        Self {
            frames_cap: NEVER_BINDS,
            frames_rate: NEVER_BINDS,
            chunks_cap: NEVER_BINDS,
            chunks_rate: NEVER_BINDS,
            bytes_cap: cap,
            bytes_rate: rate,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 这个桶按哪份形状扣（见 `Quota`）。桶自己记着，省得每次扣额度时回头查档位。
    quota: Quota,
    frames: f64,
    chunks: f64,
    bytes: f64,
    updated_at: Instant,
}

impl Bucket {
    fn new(quota: Quota, now: Instant) -> Self {
        Self {
            quota,
            frames: quota.frames_cap,
            chunks: quota.chunks_cap,
            bytes: quota.bytes_cap,
            updated_at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let quota = self.quota;
        let elapsed = now.duration_since(self.updated_at).as_secs_f64();

        self.updated_at = now;
        self.frames = (self.frames + elapsed * quota.frames_rate).min(quota.frames_cap);
        self.chunks = (self.chunks + elapsed * quota.chunks_rate).min(quota.chunks_cap);
        self.bytes = (self.bytes + elapsed * quota.bytes_rate).min(quota.bytes_cap);
    }

    /// 只问「现在还有额度吗」，**不扣**。
    ///
    /// 给每 IP 的握手闸用：被拒的尝试在拒绝那一刻就返回 429 了，不该再记一笔——
    /// 否则一次失败会被算成两次（见 `Relay::note_handshake_failure`）。
    fn available(&mut self, now: Instant) -> bool {
        self.refill(now);

        self.frames >= 1.0
    }

    /// 扣掉本次额度；不够就返回 false（连接随之关闭，所以负值不必回滚，与 CF 版一致）
    fn take(&mut self, now: Instant, frames: f64, chunks: f64, bytes: f64) -> bool {
        self.refill(now);

        self.frames -= frames;
        self.chunks -= chunks;
        self.bytes -= bytes;

        self.frames >= 0.0 && self.chunks >= 0.0 && self.bytes >= 0.0
    }

    /// 这一笔**现在**装不下时，还要等多久才装得下（`None` = 等多久都装不下）。
    ///
    /// 与 [`Self::take`] 的判据完全对称（三维都要 `>= 0`），差别是它**不扣、也不**把「已经
    /// 过去的那段时间」写回桶里：调用方拿到时长自己等，等完再真的扣一次。之所以需要它，
    /// 是因为钥匙那份预算**用完不该断连接**——回填速度本身就是限速，等一等就能接着传
    /// （见 `Relay::allow`）。
    ///
    /// `None` 只在「这一笔要的量超过桶的容量」时出现：那件事等多久都不会变，只能拒绝。
    fn wait_for(&self, now: Instant, frames: f64, chunks: f64, bytes: f64) -> Option<Duration> {
        let quota = self.quota;
        let elapsed = now.duration_since(self.updated_at).as_secs_f64();

        let seconds = |cap: f64, rate: f64, have: f64, need: f64| -> Option<f64> {
            let missing = need - (have + elapsed * rate).min(cap);

            if missing <= 0.0 {
                return Some(0.0);
            }

            // 容量比要的量还小（或者这一维根本不补水）：等多久都装不下
            if need > cap || rate <= 0.0 {
                return None;
            }

            Some(missing / rate)
        };

        let frames = seconds(quota.frames_cap, quota.frames_rate, self.frames, frames)?;
        let chunks = seconds(quota.chunks_cap, quota.chunks_rate, self.chunks, chunks)?;
        let bytes = seconds(quota.bytes_cap, quota.bytes_rate, self.bytes, bytes)?;

        Some(Duration::from_secs_f64(
            frames
                .max(chunks)
                .max(bytes)
                .min(KEY_BUDGET_WAIT_REPORT_CAP_SECS),
        ))
    }

    /// 把欠账钉在下界上（只抬不压）。
    ///
    /// 也只有**不随连接消失**的桶用得上它，而且真的用它的只有每 IP 的握手失败桶：
    /// 那个桶的欠账（负的 `frames`）换算成「这个 IP 要被挡多久」，没有下界就等于
    /// 「刷多久封多久」——拿错密码刷五分钟能封一个 IP 八个多小时，同一个 NAT（或 IPv6
    /// 的 /64）后面的人跟着连坐。钉在「一份满额度」上之后，封禁时长最多就是回填一份
    /// 额度的时间（默认约一分钟），刷得再多也只是「继续被封」。见 `README.md` 的「准入」。
    fn floor(&mut self, floor: f64) {
        self.frames = self.frames.max(floor);
    }
}

/// 一个连接被限流拦下时，是**哪个桶**不够了。
///
/// 分开是因为这两件事该说不同的话：`Connection` 是「你自己这一秒发太快了」，缓一下就好；
/// `KeyBudget` 是「这台服务器发给你的那把钥匙，额度用完得等了」——它不是这一帧的问题，
/// 而且**等不起**（见 `KEY_BUDGET_WAIT_LIMIT`：要么这一帧比整份额度还大，要么回填慢到
/// 等下去只会把连接挂死）。关闭码也跟着分开（`1008` / `4006`），客户端才能给出两句
/// 不同的话，而不是一句笼统的「格式错误」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limited {
    /// 这条连接自己的桶（按档位建的那个）
    Connection,
    /// **这把钥匙**的滚动预算（拿同一把钥匙的别的会话也在里面）。两档各有一份
    /// （`PAIR_PUBLIC_KEY_BUDGET_BYTES` / `PAIR_FULL_KEY_BUDGET_BYTES`），没配那一档就没有
    KeyBudget,
}

impl Limited {
    fn reason(self) -> &'static str {
        match self {
            Self::Connection => "rate limit exceeded",
            // 客户端只按关闭码翻译（`frame.code`），这一串只随关闭帧发出去——能看到的只有抓包
            // 与 curl 这类 WS 客户端，中继自己的日志走的是另一处 `println!`。额度用完本身只是
            // **限速**（见 `Allowance`），走到这里都是「等不起」，所以这句要带上那半句
            Self::KeyBudget => "key budget exhausted, and waiting it out would not help",
        }
    }

    /// 关这条连接时用哪个关闭码
    fn close_code(self) -> u16 {
        match self {
            Self::Connection => close_code::PROTOCOL_ERROR,
            Self::KeyBudget => close_code::KEY_BUDGET,
        }
    }
}

/// 一帧的准入结果（见 `Relay::allow`）。
///
/// 「要不要等」与「要不要断开」是两件事，所以这里有三个答案：**钥匙**那份预算用完了是
/// **等**（回填速度就是限速），而连接自己那一档、以及「等下去也没有意义」的两件事才是断开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Allowance {
    /// 现在就放行
    Pass,
    /// 现在装不下，但等这么久之后就能装下
    Wait(Duration),
    /// 这一帧发不出去：断开，`limited` 说明是哪个桶、用哪个关闭码
    Refuse(Limited),
}

impl Allowance {
    /// 把「要不要等」与**整帧**的等待预算合起来（见 [`WaitBudget`]）：预算够就返回**实际该睡**
    /// 的时长，不够就换成「等不起」的拒绝（4006）。
    ///
    /// 抽成一个方法是为了让这一步能在单测里直接跑：留在读循环里就只能搭两条真连接、靠「谁先
    /// 醒来谁拿额度」的竞态去撞，撞到还要五秒以上。
    fn step(self, budget: &mut WaitBudget) -> Self {
        match self {
            Self::Wait(wait) => match budget.take(wait) {
                Some(wait) => Self::Wait(wait),
                None => Self::Refuse(Limited::KeyBudget),
            },
            other => other,
        }
    }
}

/// 一帧的**总**等待预算（见 `KEY_BUDGET_WAIT_LIMIT`）。
///
/// 只看「这一轮要等多久」不够：同一把钥匙上若还有别的会话在持续吃额度，被压住的这条每轮
/// 都只等一小会儿（每轮都合法），却会一直睡下去——既不放行、也不 `4006`，而且内层等待不
/// 回到外层 `select!`，空闲回收在这期间也碰不到它。所以按**整帧**扣一份总预算。
///
/// 扣的必须**就是**调用方接下来会睡的那个时长（所以下限 [`KEY_BUDGET_WAIT_FLOOR`] 在这里
/// 就抬好）：不然「报出来不到 1 毫秒、实际每轮睡 1 毫秒」的那些轮次每轮都少扣一点，累计
/// 睡出来的墙钟时间会超过这份预算——预算是拿来钉住墙钟上界的，不能只钉住报出来的数字。
#[derive(Debug)]
struct WaitBudget {
    left: Duration,
}

impl WaitBudget {
    fn new(limit: Duration) -> Self {
        Self { left: limit }
    }

    /// 扣掉这一次要等的时间（返回值就是**实际该睡多久**，已经抬到 [`KEY_BUDGET_WAIT_FLOOR`]）；
    /// 不够就 `None`（调用方据此走 `4006`）。
    ///
    /// 正好等于剩余预算也放行：`KEY_BUDGET_WAIT_LIMIT` 说的是「最多等这么久」，不是
    /// 「必须小于」。
    fn take(&mut self, wait: Duration) -> Option<Duration> {
        let wait = wait.max(KEY_BUDGET_WAIT_FLOOR);

        if wait > self.left {
            return None;
        }

        self.left -= wait;

        Some(wait)
    }
}

#[derive(Default)]
struct State {
    /// `ROOM_ID` → 双人会话
    rooms: HashMap<String, PairRoom>,
    buckets: HashMap<u64, Bucket>,
    /// **每把钥匙**一份的滚动预算（钥匙序号 → 桶）。两档共用这张表，因为钥匙序号本来就
    /// 是全局唯一的（它是 `server_keys` 的下标）。
    ///
    /// 这张表的大小天然等于「部署者配了几把钥匙」，所以既不需要 TTL 也不需要清理：
    /// 桶里的令牌本来就按时间补满，留着一把没人用的钥匙的桶与不留完全等价。
    /// 挂钥匙而不是挂连接 / 房间 / IP 的理由见 `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`。
    key_budgets: HashMap<usize, Bucket>,
    /// 握手层每个 IP 的**失败**计数（`IpKey` → 桶）。只记失败，成功一次都不算。
    ///
    /// 它同时管两件事：把「一个 IP 持续刷错密码」压到 0.5 次/秒，以及把日志限频
    /// （部署者排障的唯一证据，被刷掉就等于没有）。见 `Relay::note_handshake_failure`。
    handshake_failures: HashMap<IpKey, Bucket>,
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
    /// 这把服务器钥匙已经开满了 `PAIR_MAX_SESSIONS_PER_KEY` 组会话 → 429
    KeySessionLimit,
    /// 这个 Room 里已经堆了太多条「已放行、还没走进 `admit`」的连接
    /// （`MAX_PENDING_PER_ROOM`）→ 429
    TooManyPending,
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
    /// 这次连接用的是第几把服务器钥匙。公益档的滚动预算记在钥匙上（见
    /// `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`），所以它要跟着许可一路带到 `allow`。
    key_index: usize,
    /// 这条连接来自哪个 IP（IPv6 按 /64 归并）。只在极少数「Room 被并发清掉、就地重建」
    /// 的路径上要用它，所以跟着许可一起走，不必再回头问调用方。
    ip: IpKey,
}

/// `Relay` 的全部构造参数。
///
/// 参数已经有十来项，全平铺进 `new()` 会让每个调用点都变成一串看不出含义的位置参数
/// （公益档又加了 6 项）；这里一次性收成一个结构，调用点只写关心的字段。
/// 一把「服务器钥匙」：它的 verifier 与它代表的档位（R36 + 公益档）。
///
/// 档位是**钥匙的属性**，不是会话的属性：拿哪把钥匙进来就是哪一档。所以「给不熟的人一把
/// 只能打洞的钥匙」完全落在配置里，而客户端拿到的档位是这台服务器算出来告诉它的
/// （`server.welcome` 的 `tier`）——客户端自己不用选，也无从伪造。
///
/// 同一档可以配多把（每把给一个人：换人时只撤销一把、别人照旧），但**同一把钥匙不能同时
/// 属于两档**（`server.rs` 配置阶段就拒绝），否则「这次算哪一档」会成为二义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerKey {
    pub tier: Tier,
    /// `SHA256(derive_server_token(密码))`。进程里只有摘要，没有密码原文。
    pub verifier: [u8; 32],
}

impl ServerKey {
    pub fn new(tier: Tier, password: &str) -> Self {
        Self {
            tier,
            verifier: crate::auth::server_verifier(password),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// 部署者那一档的额度（也是它的桶容量）
    pub limits: Limits,
    /// 公益档自己的额度。公益档只放行小帧（信令），所以这份值比 `limits` 小得多
    pub public_limits: Limits,
    /// 公益档**一条连接**的突发容量（`PAIR_PUBLIC_BURST_FRAMES` / `PAIR_PUBLIC_BURST_BYTES`）。
    ///
    /// 令牌桶的容量与回填速率是两个数（见 `Quota`），这是容量那一半，与
    /// `public_limits` 里的速率配成一对：一轮打洞十几帧、几 KB 要能一口气发完，否则诚实
    /// 打洞会被自己的额度掐断。
    pub public_burst_frames: f64,
    pub public_burst_bytes: f64,
    /// 公益档**每把钥匙**的滚动预算（`PAIR_PUBLIC_KEY_BUDGET_BYTES`，`None` = 不设这一层）。
    ///
    /// 桶的容量就是它本身（可以一次花完），回填速率是「容量 / 一小时」——一个数同时说明
    /// 「最多能欠多少」与「长期最快多快」。挂钥匙的理由见 `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`。
    pub public_key_budget: Option<f64>,
    /// 握手层每个 IP 每分钟允许几次**失败**（`PAIR_HANDSHAKE_FAILURES_PER_MINUTE`，
    /// `None` = 不限）
    pub handshake_failures_per_minute: Option<f64>,
    /// 部署者那一档能同时承载的会话数（`PAIR_MAX_SESSIONS`）
    pub max_sessions: usize,
    /// 能同时挂着**已放行**的 TCP 连接数上限。
    ///
    /// 名额（`max_sessions` / `max_public_sessions`）管的是**会话**，一个会话两条连接；
    /// 这里管的是**任务与内存**。它**只在请求头读完、鉴权与会话判定都过了之后**才领
    /// （见 `server.rs` 的 `handle`）：放在读请求头之前的话，随便谁开几条不发请求头的
    /// 连接就能把整池占住，让所有人拿到 503——那正是审计里那条「未鉴权占满许可池」。
    /// 读请求头那一段另有一道更宽的闸（`max_pre_handshake_connections`）。
    pub max_connections: usize,
    /// 读请求头那一段能同时挂几条连接（`DEFAULT_MAX_PRE_HANDSHAKE_CONNECTIONS`）
    ///
    /// 这一道**宽得多**，因为它只覆盖「TCP 接受了、请求头还没读完」那一段：它的职责是
    /// 给任务与内存一个硬上界，不是「够不够用」。见 `Relay::try_pre_handshake_permit`。
    pub max_pre_handshake_connections: usize,
    /// 预握手阶段**同一个来源地址**最多几条（0 = 不限）
    pub pre_handshake_per_ip: usize,
    /// 公益档能同时承载的会话数（`PAIR_MAX_PUBLIC_SESSIONS`）。0 = 公益档关闭
    pub max_public_sessions: usize,
    /// 同一把服务器钥匙最多能同时开几组会话（`PAIR_MAX_SESSIONS_PER_KEY`）。0 = 不限
    pub max_sessions_per_key: usize,
    /// 同一个 IP 最多几条公益连接（`PAIR_MAX_PUBLIC_PER_IP`）。0 = 不限
    pub max_public_per_ip: usize,
    /// 公益档的空闲回收窗口：多久没收到任何入站消息就断开。`None` = 不回收
    pub public_window: Option<Duration>,
    /// 完全档的空闲回收窗口：判据与公益档那条一样。`None` = 不回收
    pub full_window: Option<Duration>,
    /// 多久没有消息的连接可以被新连接顶替
    pub stale_after: Duration,
    /// `server.welcome` 里附带的 ICE 服务器（`PAIR_ICE_SERVERS`，可选，原样透传）
    pub ice_servers: Option<serde_json::Value>,
    /// 内置 STUN 的 UDP 端口（`None` = 没有内置 STUN，见 `stun.rs`）。有它而 `ice_servers`
    /// 为空时，welcome 里广告 `stun:<客户端连进来用的主机名>:<端口>`。
    pub stun_port: Option<u16>,
    /// 服务器钥匙表（R36）：谁能连上这台服务器、以及进来之后算哪一档，全看它。
    ///
    /// `PAIR_SERVER_PASSWORD`（完全档）与 `PAIR_PUBLIC_SERVER_PASSWORD`（公益档）都接受
    /// `;` 分隔的多把；空表 = 谁都不认（会话层单测用得到，真部署走 `server.rs`，那里
    /// 完全档是**必填**的）。
    pub server_keys: Vec<ServerKey>,
    /// 完全档**每把钥匙**的滚动预算（`PAIR_FULL_KEY_BUDGET_BYTES`，`None` = 不设这一层）
    pub full_key_budget: Option<f64>,
    /// 每把钥匙那份预算按多长时间回填（默认 [`KEY_BUDGET_WINDOW_SECS`]，一小时）。
    ///
    /// 它同时就是长期速率：桶先装一次量、之后按这个窗口回填（用完是**限速**，不是断开，
    /// 见 `Relay::allow`）。写成一个可改的字段而不是直接用常量，是为了让测试能把窗口压到
    /// 一秒，把那条等待的路跑成一条快用例；真部署永远是这个常量。
    pub key_budget_window: Duration,
    /// 限时 TURN 凭据的共享密钥（`PAIR_TURN_SECRET`）。`None` = `PAIR_ICE_SERVERS` 原样透传，
    /// 也就是用那份配置里写死的静态凭据（与旧版行为一致）
    pub turn_secret: Option<String>,
    /// 限时 TURN 凭据的有效期（`PAIR_TURN_TTL_SECS`）。只在配了 `turn_secret` 时有意义
    pub turn_ttl: Duration,
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
            public_burst_frames: DEFAULT_PUBLIC_BURST_FRAMES,
            public_burst_bytes: DEFAULT_PUBLIC_BURST_BYTES,
            public_key_budget: Some(DEFAULT_PUBLIC_KEY_BUDGET_BYTES),
            handshake_failures_per_minute: Some(DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE),
            max_sessions: protocol::DEFAULT_MAX_SESSIONS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_pre_handshake_connections: DEFAULT_MAX_PRE_HANDSHAKE_CONNECTIONS,
            pre_handshake_per_ip: DEFAULT_PRE_HANDSHAKE_PER_IP,
            max_public_sessions: protocol::DEFAULT_MAX_PUBLIC_SESSIONS,
            max_sessions_per_key: DEFAULT_MAX_SESSIONS_PER_KEY,
            max_public_per_ip: protocol::DEFAULT_MAX_PUBLIC_PER_IP,
            public_window: Some(Duration::from_secs(protocol::DEFAULT_PUBLIC_WINDOW_SECS)),
            full_window: Some(Duration::from_secs(DEFAULT_FULL_WINDOW_SECS)),
            stale_after: Duration::from_millis(protocol::DEFAULT_STALE_AFTER_MS),
            ice_servers: None,
            stun_port: None,
            server_keys: Vec::new(),
            full_key_budget: Some(DEFAULT_FULL_KEY_BUDGET_BYTES),
            key_budget_window: Duration::from_secs_f64(KEY_BUDGET_WINDOW_SECS),
            turn_secret: None,
            turn_ttl: Duration::from_secs(protocol::DEFAULT_TURN_TTL_SECS),
            trust_proxy: false,
        }
    }
}

pub struct Relay {
    options: RelayOptions,
    next_id: AtomicU64,
    state: Mutex<State>,
    /// 能同时挂着**已放行**的连接数（见 `RelayOptions::max_connections`）。
    ///
    /// 放在会话层是为了让「准入闸」与「配置」只有一处映射：`server.rs` 拿它当闸门，别的
    /// 调用点（`main.rs`、集成测试）一个字都不用改。
    connections: Arc<Semaphore>,
    /// 读请求头那一段的闸（见 `RelayOptions::max_pre_handshake_connections`）。
    ///
    /// 与 `connections` 分开是这件事的关键：把两者合成一道，未鉴权的连接就会占掉真实额度
    /// ——那正是审计里 P0-1 那条。这一道只管「任务与内存」，所以它可以（也应该）很宽。
    pre_handshake: Arc<Semaphore>,
    /// 预握手阶段每个来源地址的计数（`IpKey` → 条数）。
    ///
    /// 用 `std::sync::Mutex` 而不是会话层那个 `tokio::sync::Mutex`：这一段里没有 `await`，
    /// 而**归还**发生在 `Drop` 里——`Drop` 不能 await，拿异步锁就等于把「漏一个许可」变成
    /// 一个迟早会发生的必然（那道闸会越来越窄，最后连诚实连接都进不来）。零就删键，别让
    /// 扫描器留下一堆只用一次的小条目。
    pre_handshake_per_ip: PreHandshakeCounts,
    /// 「连接数已达上限」那行日志上一次是什么时候打的（见 `CONNECTION_LIMIT_NOTICE_INTERVAL`）。
    connection_limit_notice: Mutex<Option<Instant>>,
}

/// 「读请求头」那一段的许可，由 [`Relay::try_pre_handshake_permit`] 签发。
///
/// 两个额度都在 `Drop` 里归还。这条路上有十来处提前返回（路径不对、协议不对、密码不对、
/// 格式不对……），靠调用方逐个显式释放迟早会漏掉一处——而漏掉一处的后果是**永久**少一个
/// 许可：那道闸会越来越窄，最后连诚实连接都进不来，且没有任何日志能看出原因。
pub struct PreHandshakePermit {
    /// 总闸那一个。它自己会在 `Drop` 里归还，这里只需要留住它
    _permit: OwnedSemaphorePermit,
    /// 这个来源地址那一份计数（`None` = 这一档不限每 IP）
    per_ip: Option<(PreHandshakeCounts, IpKey)>,
}

/// 预握手阶段每个来源地址的计数表（见 `Relay::pre_handshake_per_ip`）。
type PreHandshakeCounts = Arc<SyncMutex<HashMap<IpKey, usize>>>;

impl Drop for PreHandshakePermit {
    fn drop(&mut self) {
        let Some((counts, ip)) = self.per_ip.take() else {
            return;
        };

        let mut counts = counts.lock().unwrap_or_else(|error| error.into_inner());

        if let Some(count) = counts.get_mut(&ip) {
            *count = count.saturating_sub(1);

            if *count == 0 {
                counts.remove(&ip);
            }
        }
    }
}

impl Relay {
    pub fn new(options: RelayOptions) -> Arc<Self> {
        Arc::new(Self {
            connections: Arc::new(Semaphore::new(options.max_connections)),
            pre_handshake: Arc::new(Semaphore::new(options.max_pre_handshake_connections)),
            pre_handshake_per_ip: Arc::new(SyncMutex::new(HashMap::new())),
            options,
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
            connection_limit_notice: Mutex::new(None),
        })
    }

    /// 读请求头之前先拿一张「预握手」的许可。`None` = 该回 503（**不排队**）。
    ///
    /// 两道闸的顺序是这件事的要点：**先**过这一道（宽、短命、读完请求头就还），**后**过
    /// `try_connection_permit`（窄、与真实容量同阶、持到连接结束）。反过来就是「随便谁开
    /// 几条不发请求头的 TCP 就能把真实额度占满」——那时所有人（包括部署者自己）都拿到
    /// 503，而攻击者什么都不用做。
    pub fn try_pre_handshake_permit(&self, peer: IpAddr) -> Option<PreHandshakePermit> {
        let permit = Arc::clone(&self.pre_handshake).try_acquire_owned().ok()?;
        let limit = self.options.pre_handshake_per_ip;

        if limit == 0 {
            return Some(PreHandshakePermit {
                _permit: permit,
                per_ip: None,
            });
        }

        let ip = IpKey::from_addr(peer);
        let counts = Arc::clone(&self.pre_handshake_per_ip);

        {
            // 中毒只可能是「锁里面 panic 过」；计数本身没有不变量可破坏，恢复原值即可，
            // 不值得把一次连接失败升级成整台服务器挂掉
            let mut counts = counts.lock().unwrap_or_else(|error| error.into_inner());
            let count = counts.entry(ip).or_default();

            if *count >= limit {
                // 满了：这一条连预握手池都不进（许可随之在这里归还）。等下一次再来。
                return None;
            }

            *count += 1;
        }

        Some(PreHandshakePermit {
            _permit: permit,
            per_ip: Some((counts, ip)),
        })
    }

    /// 能同时挂几条**已放行**的连接（横幅要如实印出来）
    pub fn max_connections(&self) -> usize {
        self.options.max_connections
    }

    /// 读请求头那一段能同时挂几条（横幅要如实印出来）
    pub fn max_pre_handshake_connections(&self) -> usize {
        self.options.max_pre_handshake_connections
    }

    /// 拿一张「能挂一条连接」的许可。`None` = 已经打到上限（调用方该回一个 503 并断开）。
    ///
    /// 不排队等：等就等于把已经接受的 socket 堆在队列里，而那正是这道闸要防的东西。
    pub fn try_connection_permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.connections).try_acquire_owned().ok()
    }

    /// 「连接数已达上限」这一行现在该不该打（每 `CONNECTION_LIMIT_NOTICE_INTERVAL` 一行）。
    pub async fn note_connection_limit(&self) -> bool {
        let mut last = self.connection_limit_notice.lock().await;
        let now = Instant::now();

        match *last {
            Some(at) if now.duration_since(at) < CONNECTION_LIMIT_NOTICE_INTERVAL => false,
            _ => {
                *last = Some(now);

                true
            }
        }
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
        self.options.max_public_sessions > 0
            && self
                .options
                .server_keys
                .iter()
                .any(|key| key.tier == Tier::Public)
    }

    /// 这一档配了几把钥匙（启动横幅与 `/health` 共用一份口径）。只数把数，不涉及内容。
    pub fn server_key_count(&self, tier: Tier) -> usize {
        self.options
            .server_keys
            .iter()
            .filter(|key| key.tier == tier)
            .count()
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

    /// 这一档**一条连接**的令牌桶形状。
    ///
    /// 完全档就是 `Limits` 自己（容量 = 速率 = 每秒上限，与 CF 版逐条一致）；公益档把突发
    /// 与持续拆成两个数——诚实流量是「一轮几 KB 的突发 + 长期静默」，只用一个数就只能
    /// 二选一（小到装不下一轮打洞，或者大到等于没有上限）。
    fn connection_quota(&self, tier: Tier) -> Quota {
        match tier {
            Tier::Full => Quota::steady(self.options.limits),
            Tier::Public => Quota::public_connection(
                self.options.public_limits,
                self.options.public_burst_frames,
                self.options.public_burst_bytes,
            ),
        }
    }

    /// 这一档**每把钥匙**的滚动预算桶形状（`None` = 这一档不设这一层）。
    ///
    /// 两档共用一份记账（`State::key_budgets`），只是额度各配各的：公益档是「一轮打洞」
    /// 的量级（16 MiB），完全档要承载聊天 / 语音 / 附件分片，所以大得多（2 GiB）。没有这一层
    /// 时，一把流出去的钥匙就是一条无限的中转链路——它的代价由部署者的带宽与账单承担。
    fn key_budget_quota(&self, tier: Tier) -> Option<Quota> {
        let budget = match tier {
            Tier::Full => self.options.full_key_budget,
            Tier::Public => self.options.public_key_budget,
        }?;

        // 窗口写 0 会算出 `inf` 的回填速度，那一层就**静默失效**了（限额全没），所以兜到
        // 一秒：它不是部署配置（真部署固定一小时），只有测试会改它。
        let window = self.options.key_budget_window.as_secs_f64().max(1.0);

        Some(Quota::bytes_only(budget, budget / window))
    }

    /// 这一档的空闲回收窗口（`None` = 不回收）。判据两档一样，只是窗口各配各的。
    fn window_for(&self, tier: Tier) -> Option<Duration> {
        match tier {
            Tier::Full => self.options.full_window,
            Tier::Public => self.options.public_window,
        }
    }

    /// 这次连接带来的服务器凭据算哪一档（R36 + 公益档）。`None` = 凭据不对，拒绝。
    ///
    /// 恒定时间比较、与长度无关的旁路不成立（两边都是 32 字节摘要）。**每一把都要比完
    /// 再决定**：命中就提前返回会让「命中了哪一把、哪一档」通过时间差漏出去（钥匙可以配
    /// 多把之后这一条更要紧：提前返回连「表里有没有这一把」都跟着搜索顺序漏出去）。
    ///
    /// 公益档被关掉（名额 0）时**故意**不认那把钥匙：它就等于「这台服务器没有公益档」，
    /// 于是那些人拿到的是 403「服务器密码不正确」，而不是一条会让客户端无限退避重试的
    /// 503「会话已满」——后者说的是一件没发生的事（名额根本没被占满）。
    pub fn classify_server_token(&self, token: &str) -> Option<(Tier, usize)> {
        let verifier = auth_verifier(token);
        let public_open = self.has_public_tier();
        let mut matched = None;

        for (index, key) in self.options.server_keys.iter().enumerate() {
            // 公益档关掉时那把钥匙不参与比较：它等于「这台服务器没有公益档」，判定要
            // 和「压根没配过」完全一样（见上面那条 403 与 503 的区别）。
            if key.tier == Tier::Public && !public_open {
                continue;
            }

            if constant_time_eq(&verifier, &key.verifier) {
                // 序号一起带出去：公益档的滚动预算记在**钥匙**上（见
                // `DEFAULT_PUBLIC_KEY_BUDGET_BYTES`），后面每一帧都要按它记账。
                matched = Some((key.tier, index));
            }
        }

        matched
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
    ///
    /// 配了 `PAIR_TURN_SECRET` 时，`turn:` 条目的凭据换成**这一次连接现签的限时凭据**
    /// （见 `sign_turn_credentials`）：那份静态凭据是长期有效的，一旦出现在任何一份日志或
    /// 聊天记录里，别人就能一直拿它打（流量算部署者的）。客户端那边零改动——`iceServers`
    /// 在协议里是无类型透传，它只是把这一份转交给 WebRTC。
    pub fn ice_servers_for(
        &self,
        host: Option<&str>,
        tier: Tier,
        key_index: usize,
    ) -> Option<serde_json::Value> {
        if let Some(servers) = &self.options.ice_servers {
            let servers = match &self.options.turn_secret {
                // 形状不对（不是数组）时退回原样：凭据该换没换成了一件事，把整份清单吃掉是
                // 另一件更糟的事（客户端会一个 STUN 都拿不到，跨网络直接打不通）
                Some(secret) => {
                    sign_turn_credentials(servers, secret, self.options.turn_ttl, key_index)
                        .unwrap_or_else(|| servers.clone())
                }
                None => servers.clone(),
            };

            return match tier {
                Tier::Full => Some(servers),
                Tier::Public => stun_only(&servers),
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
        key_index: usize,
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

                // 已经在握手里的连接也要封顶：不封顶时同一个 Room 能被堆上任意多条这样的
                // 连接，每条占一个任务、一份 8 KiB 的请求头缓冲（还在 10 秒的握手超时
                // 里），而它们的许可**都算进容量**——既拖住自己，也拖住别人。
                // 4 条 = 正常用法的两倍余量（两个人握手 + 一次重连重叠）。
                if room.pending >= MAX_PENDING_PER_ROOM {
                    return Err(RoomRejection::TooManyPending);
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

                // 「一把钥匙一个人」：同一把钥匙同时能开几组会话。它挡的是「一把流出去的
                // 钥匙把整档名额吃光」（撤销那把钥匙之前，最坏也就占掉这一个数）。
                //
                // 只挡**新建**：同一个会话的第二条连接走上面那一支，不受影响——两个人用
                // 同一把钥匙进同一个 Room 时，这里数出来仍然只是一组。
                if self.options.max_sessions_per_key > 0 {
                    let used = state
                        .rooms
                        .values()
                        .filter(|room| room.key_index == key_index)
                        .count();

                    if used >= self.options.max_sessions_per_key {
                        return Err(RoomRejection::KeySessionLimit);
                    }
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
                        key_index,
                        clients: HashMap::new(),
                        pending: 1,
                        created_at: now,
                        last_active: now,
                        forwarded_frames: 0,
                        forwarded_bytes: 0,
                    },
                );
            }
        }

        Ok(Reservation {
            room_id: room_id.to_string(),
            auth_hash,
            tier,
            key_index,
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
        let key_index = reservation.key_index;

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
        //
        // 公益档那三个数都按自己的档位取（`PUBLIC_WS_MESSAGE_SIZE` /
        // `PUBLIC_WS_WRITE_BUFFER` / `PUBLIC_OUTBOUND_QUEUE`）：那一档只转发 64 KiB 以下的
        // 信令帧，所以它的单连接内存从完全档的十几 MiB 降到 1 MiB 以下。「一堆公益连接把
        // 宿主机内存吃光、把部署者自己那一档一起搞死」这条路因此被堵住——这是**按档位的
        // 硬隔离**，不靠额度（额度是策略，内存是物理）。
        // `WebSocketConfig` 是 non_exhaustive，只能先取默认值再改字段
        let mut config = WebSocketConfig::default();

        let (read_limit, write_buffer, queue) = match tier {
            Tier::Full => (
                MAX_BINARY_FRAME_SIZE * 8,
                // 写缓冲也设上限：默认是无限，碰到对端停摆时同样会吃内存（真正的流控靠
                // 上面那个有界队列，这里只是兜底）
                4 * MAX_BINARY_FRAME_SIZE,
                OUTBOUND_QUEUE,
            ),
            Tier::Public => (
                PUBLIC_WS_MESSAGE_SIZE,
                PUBLIC_WS_WRITE_BUFFER,
                PUBLIC_OUTBOUND_QUEUE,
            ),
        };

        config.max_message_size = Some(read_limit);
        config.max_frame_size = Some(read_limit);
        config.max_write_buffer_size = write_buffer;

        let mut websocket =
            WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
        let (sender, receiver) = mpsc::channel::<Message>(queue);

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
                    ice_servers: self.ice_servers_for(head.header("host"), tier, key_index),
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

        // 空闲回收（见 `protocol.rs` 的 `DEFAULT_PUBLIC_WINDOW_SECS` / `DEFAULT_FULL_WINDOW_SECS`）。
        //
        // 它是**空闲回收器**，不是「打洞截止时间」：中继看不到 DataChannel 有没有建立
        // 成功（信令是密文），所以任何「到点硬断」都会掐断已经直连成功、正在正常使用的
        // 会话——而中继一断，客户端是整条会话重启、直连也跟着重来。判据是「多久没收到
        // **任何**入站消息」，诚实客户端每 60 秒发一次 WebSocket Ping。两档都有它，只是
        // 窗口不同：完全档那条要挡的是「对端机器睡眠 / 拔网线之后留下的僵尸连接」，那会让
        // 一条什么都没在传的连接一直占着会话与连接额度。
        let window = self.window_for(tier);
        let mut idle_deadline = window.map(|window| tokio::time::Instant::now() + window);

        'session: loop {
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
                        close_code::IDLE,
                        "idle timeout",
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
                    let frame_bytes = bytes.len() as f64;

                    let mut limited = None;
                    // 整帧的总等待预算，不是单轮：同一把钥匙上的别的会话在抢额度时，每一轮
                    // 都等得着、却能一直睡下去（见 `WaitBudget`）
                    let mut wait_budget = WaitBudget::new(KEY_BUDGET_WAIT_LIMIT);

                    // 额度：装得下就转发，装不下就**等**（钥匙那份预算用完了只是限速——回填
                    // 速度就是速度上限；见 `Allowance`）。等待期间**不读**这条连接，TCP 背压
                    // 自然会把发送方压到同一个速度，这正是限速本身。
                    loop {
                        // 「整帧等过头也是等不起」这一步在 `Allowance::step` 里：预算不够就
                        // 直接换成 4006（再等下去，客户端会先以「发送超时」的名义把会话重启，
                        // 原因反而看不见，见 `KEY_BUDGET_WAIT_LIMIT`）
                        let allowance = self
                            .allow(id, tier, key_index, 1.0, chunks, frame_bytes)
                            .await
                            .step(&mut wait_budget);

                        match allowance {
                            Allowance::Pass => break,
                            Allowance::Refuse(limit) => {
                                limited = Some(limit);

                                break;
                            }
                            Allowance::Wait(wait) => {
                                // `wait` 就是实际该睡的时长（下限已经在 `take` 里抬过）。
                                // 等的时候只认「被顶替」这一个信号：`idle` 交给下一轮
                                // `select!`（等待有上界，见 `WaitBudget`）
                                tokio::select! {
                                    _ = &mut ejected => break 'session,
                                    _ = tokio::time::sleep(wait) => {}
                                }
                            }
                        }
                    }

                    if let Some(limit) = limited {
                        // 「这把钥匙的预算用完了」和「你自己发太快」是两件事，而关闭帧里的
                        // 原因客户端只按关闭码翻译、看不到。这里替部署者记一行：不然
                        // 「我的额度突然没了」在日志里只剩一条「会话已释放」，谁也说不清
                        // 是哪把钥匙、更看不出是不是有人在夹带。
                        //
                        // 这一行只在「等不起」时才出现（见 `KEY_BUDGET_WAIT_LIMIT`）：额度
                        // 用完本身是**限速**（等一等接着传，连日志都不会有），这里说明的是
                        // 「等下去也没有意义」——回填速度相对这一帧太小，再等只是把连接挂死。
                        if limit == Limited::KeyBudget {
                            println!(
                                "[{}] {}第 {} 把钥匙的额度已用完，而且等不起（device {device_id}）：\
                                 {} 秒内等不到这一帧——这一帧比整份额度还大，\
                                 或者回填慢到等下去只会把连接挂死。这条连接被关掉；\
                                 拿同一把钥匙的别的会话一样发不出去。要么等额度回填，\
                                 要么换一把钥匙，要么把额度（PAIR_FULL_KEY_BUDGET_BYTES / \
                                 PAIR_PUBLIC_KEY_BUDGET_BYTES）调大",
                                room_fingerprint(&room_id),
                                tier_text(tier),
                                key_index + 1,
                                KEY_BUDGET_WAIT_LIMIT.as_secs()
                            );
                        }

                        let _ = sender.try_send(Message::Close(Some(close_frame(
                            limit.close_code(),
                            limit.reason(),
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
                        key_index: reservation.key_index,
                        clients: HashMap::new(),
                        pending: 0,
                        created_at: now,
                        last_active: now,
                        forwarded_frames: 0,
                        forwarded_bytes: 0,
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

        state
            .buckets
            .insert(id, Bucket::new(self.connection_quota(tier), now));

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

    /// 这一帧能不能放行、要不要等、还是该断开（见 `Allowance`）。
    ///
    /// 两条闸的分工：
    ///
    /// - **这条连接自己**那一档（按档位建、容量 = 速率）仍然**断开**。它是「这一秒发太多」
    ///   的自保闸，诚实客户端按自己的节奏走、碰不到它；而在这里等待也没有意义——一秒之后
    ///   它还是发这么快。
    /// - **这把钥匙**的滚动预算改成**等待**。它的回填速度本身就是限速（那个桶按
    ///   `key_budget_window` 回填，默认一小时），所以「等一等再接着发」正是要的行为：
    ///   大文件因此能慢慢传完，而不是被 `4006` 断开。只有「等下去也没有意义」的两件事仍然
    ///   拒绝——这一帧要的量超过整个桶的容量，或者要等超过 `KEY_BUDGET_WAIT_LIMIT`。
    ///
    /// 两笔额度**一起扣**：账目要等于真的转发出去的字节，所以等待那条路上什么都没先扣
    /// （否则重试一次就扣两次）。
    async fn allow(
        &self,
        id: u64,
        tier: Tier,
        key_index: usize,
        frames: f64,
        chunks: f64,
        bytes: f64,
    ) -> Allowance {
        let budget = self.key_budget_quota(tier);
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let State {
            buckets,
            key_budgets,
            ..
        } = &mut *state;

        // 已经不在名单里的连接不再有桶：`entry().or_insert_with()` 会把桶重建出来，
        // 被摘掉的对端再发一帧就永久留下一条残留。这里直接拒绝，让读循环退出。
        let Some(connection) = buckets.get_mut(&id) else {
            return Allowance::Refuse(Limited::Connection);
        };

        if connection.wait_for(now, frames, chunks, bytes) != Some(Duration::ZERO) {
            return Allowance::Refuse(Limited::Connection);
        }

        // 这一档没配这一层（`None`）到这儿就是放行
        let Some(quota) = budget else {
            connection.take(now, frames, chunks, bytes);

            return Allowance::Pass;
        };

        let key = key_budgets
            .entry(key_index)
            .or_insert_with(|| Bucket::new(quota, now));
        let wait = key.wait_for(now, frames, chunks, bytes);

        if wait == Some(Duration::ZERO) {
            connection.take(now, frames, chunks, bytes);
            key.take(now, frames, chunks, bytes);

            return Allowance::Pass;
        }

        match wait {
            Some(wait) if wait <= KEY_BUDGET_WAIT_LIMIT => Allowance::Wait(wait),
            // `None` = 额度配得比一帧还小；`Some(更久)` = 等不起。两者都只能断开
            _ => Allowance::Refuse(Limited::KeyBudget),
        }
    }

    /// 这个 IP 现在还能不能做一次握手（`PAIR_HANDSHAKE_FAILURES_PER_MINUTE`）。
    ///
    /// 只问不扣：被挡下的这一次本身也已经被记过一次了（它走到这里之前的那次失败调的是
    /// `note_handshake_failure`），这里再扣就会一次失败算两次。还没失败过的 IP 连桶都没
    /// 建，直接放行。
    pub async fn handshake_allowed(&self, ip: IpKey) -> bool {
        if self.options.handshake_failures_per_minute.is_none() {
            return true;
        }

        let mut state = self.state.lock().await;
        let now = Instant::now();

        match state.handshake_failures.get_mut(&ip) {
            Some(bucket) => bucket.available(now),
            None => true,
        }
    }

    /// 记一次握手失败（格式不对 / 密码不对 / 被挡下），并回答「这一行日志该不该打」。
    ///
    /// 返回值同时承担两件事：日志限频（刷错密码时每个 IP 每分钟最多 30 行——部署者排障的
    /// 唯一证据就是日志，被刷掉等于没有）与「这个 IP 是不是该被 429 了」（下一次握手先问
    /// `handshake_allowed`）。关掉这一项（`None`）时永远返回 `true`，也就是回到从前那样
    /// 每拒一次写一行。
    ///
    /// 欠账跟着 [`Bucket::floor`] 钉了下界，所以「被挡多久」有个上界（约一分钟），刷得
    /// 再多也只是「继续被封」。
    pub async fn note_handshake_failure(&self, ip: IpKey) -> bool {
        let Some(limit) = self.options.handshake_failures_per_minute else {
            return true;
        };

        let mut state = self.state.lock().await;
        let now = Instant::now();

        // 表大了就顺手清一遍：回填到顶的条目代表「这个 IP 已经安静了一次失败该回填的
        // 时间」，留着就是一份只涨不跌的表（扫描器会带来一大堆只用一次的 IP）。
        // 只在**要插一个新地址**时扫：表停在高位时（IPv6 下现实）老地址反复失败每次都扫
        // 一遍全表，而那正是被刷的时刻——扫描本身成了放大器。正常部署里这张表只有几项，
        // 连扫都不会扫。
        if !state.handshake_failures.contains_key(&ip)
            && state.handshake_failures.len() >= HANDSHAKE_FAILURE_TABLE_SWEEP
        {
            state.handshake_failures.retain(|_, bucket| {
                bucket.refill(now);

                bucket.frames < limit
            });
        }

        let bucket = state
            .handshake_failures
            .entry(ip)
            .or_insert_with(|| Bucket::new(Quota::frames_only(limit, limit / 60.0), now));

        let logged = bucket.take(now, 1.0, 0.0, 0.0);

        bucket.floor(-limit);

        logged
    }

    /// 只转发给**同一个 Room** 的对端，不回发给发送者，也不遍历别的 Room（§15）。
    ///
    /// 队列是有限的：对端消费不过来时这里会 await 等空位（反压回发送方的 TCP），
    /// 而不是把帧堆在内存里。真被拖过 `FORWARD_TIMEOUT` 就把那个对端摘掉（只摘那一条，
    /// 而且只摘在它自己的 Room 里）——一条停摆的连接不该拖死整台中继。
    async fn forward(&self, room_id: &str, from: u64, message: Message) {
        let targets: Vec<(String, u64, mpsc::Sender<Message>)> = {
            let mut state = self.state.lock().await;

            match state.rooms.get_mut(room_id) {
                None => Vec::new(),
                Some(room) => {
                    let targets: Vec<(String, u64, mpsc::Sender<Message>)> = room
                        .clients
                        .iter()
                        .filter(|(_, entry)| entry.id != from)
                        .map(|(device_id, entry)| {
                            (device_id.clone(), entry.id, entry.sender.clone())
                        })
                        .collect();

                    // 真的有人接才算「转发了一帧」。这条统计是给部署者事后看「哪个会话 /
                    // 哪把钥匙在夹带」用的（`sweep` 那行日志），把发给空气的帧也算进去
                    // 会让它对不上真实出口流量，作为证据就没用了。
                    if !targets.is_empty() {
                        room.forwarded_frames += 1;
                        room.forwarded_bytes += message.len() as u64;
                    }

                    targets
                }
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
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(v6)),
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
/// 取**所有头行里的最后一项**：那是我们信任的这一跳自己写上去的。两件事缺一不可，头被
/// 拆成多行只是其中一半：Caddy 是 append（客户端自带的前缀会留在前面），而 Go 的
/// `net/http` 做 append 时写成**另一行**——只看第一行就等于把客户端伪造的值当成了真实
/// IP，每 IP 限额被直接绕开（一台机器换着假 IP 就能吃满公益名额）。Caddyfile 那边同时把
/// 这一项钉死成 `{remote_host}`，两层各管一段：Caddy 永远只写一项，中继永远只认最后一项。
///
/// 取的是**字面意义上的最后一项**（空项也算最后一项）：某一跳留了个尾逗号这种邋遢形状
/// 会让解析失败、退回对端地址——从严那一侧。要是反过来去往前找最后一个非空项，就等于把
/// 客户端写的那一项又捡回来了（那正是这条修复要堵的东西）。
pub fn client_ip(peer: SocketAddr, forwarded_for: &[&str], trust_proxy: bool) -> IpKey {
    if trust_proxy && is_private_or_loopback(peer.ip()) {
        if let Some(address) = forwarded_for
            .iter()
            .flat_map(|value| value.split(','))
            .next_back()
            .map(str::trim)
            .and_then(|last| last.parse::<IpAddr>().ok())
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

/// 日志里那一档怎么称呼（`Tier::as_str` 是给协议用的 `full` / `public`，人读的话另写一套）。
fn tier_text(tier: Tier) -> &'static str {
    match tier {
        Tier::Full => "完全档",
        Tier::Public => "公益档",
    }
}

/// 把一份 `iceServers` 里的 `turn:` 条目换成**限时凭据**（coturn 的 REST API 形状）。
///
/// `username` 是 `<过期 unix 秒>:<标识>`，`credential` 是
/// `base64(HMAC-SHA1(共享密钥, username))`。coturn 只在**新建分配**时校验时间戳与签名，
/// 会话内缓存 hmackey，所以同一份凭据在整个会话里一直有效——这正是想要的：TTL 只要长过
/// 会话寿命就不会中途失效（见 `DEFAULT_TURN_TTL_SECS`）。
///
/// 标识用「第几把钥匙」（1 起）：它不带任何秘密，只是让部署者在 coturn 的日志里看得出
/// 「这份凭据是谁在用」。**不碰 `stun:` 条目**（STUN 本来就不鉴权，给它加凭据没有意义，
/// 还会把一份干净的探针配置搞得看不懂）。
///
/// 返回 `None` 只在「`servers` 不是数组」时——调用方据此退回原样透传：凭据该换没换成是一
/// 件事，把整份清单吃掉是另一件更糟的事（客户端会一个 STUN 都拿不到，跨网络直接打不通）。
fn sign_turn_credentials(
    servers: &serde_json::Value,
    secret: &str,
    ttl: Duration,
    key_index: usize,
) -> Option<serde_json::Value> {
    let entries = servers.as_array()?;
    let expire = unix_now() + ttl.as_secs();
    let username = format!("{expire}:{}", key_index + 1);
    let credential = turn_credential(secret, &username)?;
    let mut signed = Vec::with_capacity(entries.len());

    for entry in entries {
        let urls = entry
            .get("urls")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let carries_turn = urls_of(&urls).is_some_and(|urls| {
            urls.iter()
                .any(|url| url.starts_with("turn:") || url.starts_with("turns:"))
        });

        if !carries_turn {
            signed.push(entry.clone());

            continue;
        }

        let mut object = entry.as_object().cloned().unwrap_or_default();

        object.insert("urls".to_string(), urls);
        object.insert(
            "username".to_string(),
            serde_json::Value::String(username.clone()),
        );
        object.insert(
            "credential".to_string(),
            serde_json::Value::String(credential.clone()),
        );

        signed.push(serde_json::Value::Object(object));
    }

    Some(serde_json::Value::Array(signed))
}

/// coturn REST API 的那一份凭据：`base64(HMAC-SHA1(共享密钥, username))`。
///
/// 标准 base64（带 `+/=`），与 coturn 的 `--use-auth-secret` 实现一致。
fn turn_credential(secret: &str, username: &str) -> Option<String> {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).ok()?;

    mac.update(username.as_bytes());

    Some(BASE64.encode(mac.finalize().into_bytes()))
}

/// 当前 unix 秒。时钟被拨到 1970 之前（不该发生）时按 0 算：凭据会立刻过期，而不是算出一
/// 个荒唐的远期值。
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
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
    let key_index = room.key_index;
    let forwarded_frames = room.forwarded_frames;
    let forwarded_bytes = room.forwarded_bytes;

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

    // 「第几把钥匙」与「转发了多少」是给部署者事后看的两条线索：中继看不到载荷
    // （信令是端到端加密的），所以「哪个会话在夹带、是哪把钥匙带来的」只能靠这两项推断。
    // 前者是创建这个会话的那把钥匙，同一个会话里的两个人可以拿不同的钥匙。
    println!(
        "[{}] 双人会话已释放（{} 档，钥匙 #{}，转发 {} 帧 / {:.1} KB，\
         存活 {:.0}s，最后活动在 {:.0}s 前）",
        room_fingerprint(room_id),
        tier.as_str(),
        key_index + 1,
        forwarded_frames,
        forwarded_bytes as f64 / 1024.0,
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
    /// 第二把**公益**钥匙：验「预算记在钥匙上，不是记在档位上」
    const SECOND_PUBLIC_KEY: &str = "relay-unit-tests-second-public-key-1";

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
        /// 公益档一条连接的突发容量（`None` = 用缺省：24 帧 / 64 KiB 足够一轮打洞）
        public_burst_frames: Option<f64>,
        public_burst_bytes: Option<f64>,
        /// 公益档每把钥匙的滚动预算（`None` = 用缺省，也就是真的开着）
        public_key_budget: Option<f64>,
        /// 完全档每把钥匙的滚动预算（`None` = 用缺省 2 GiB；`Some` 里 `0` = 关掉这一层）
        full_key_budget: Option<f64>,
        /// 完全档的空闲回收窗口（`None` = 用缺省 300 秒）
        full_window: Option<Option<Duration>>,
        /// 同一把钥匙的会话上限（`None` = 用缺省 4）
        max_sessions_per_key: Option<usize>,
        /// 预握手总闸（`None` = 用缺省 512）
        max_pre_handshake_connections: Option<usize>,
        /// 预握手每 IP（`None` = 用缺省 64）
        pre_handshake_per_ip: Option<usize>,
        /// 限时 TURN 凭据的共享密钥（`None` = 原样透传）
        turn_secret: Option<&'static str>,
        /// 限时 TURN 凭据的有效期（`None` = 用缺省 24 小时）
        turn_ttl: Option<Duration>,
        /// 握手层每 IP 每分钟的失败次数（`None` = 用缺省的那个 30）
        handshake_failures_per_minute: Option<f64>,
        stale_after: Duration,
        ice_servers: Option<serde_json::Value>,
        stun_port: Option<u16>,
        /// 公益档配不配那把固定钥匙（`PUBLIC_PASSWORD`）
        public_tier: bool,
        /// 额外的服务器钥匙（`(档位, 密码)`）：多把钥匙那几条用例要它
        extra_keys: Vec<(Tier, &'static str)>,
        trust_proxy: bool,
        /// 钥匙那份预算的回填窗口（真部署固定是一小时；单测压到一秒，好把「等待」那条路
        /// 跑成一条快用例）
        key_budget_window: Option<Duration>,
    }

    fn relay_with(options: Options) -> Arc<Relay> {
        let defaults = RelayOptions::default();

        Relay::new(RelayOptions {
            limits: Limits::default(),
            key_budget_window: options
                .key_budget_window
                .unwrap_or(defaults.key_budget_window),
            public_limits: defaults.public_limits,
            public_burst_frames: options
                .public_burst_frames
                .unwrap_or(defaults.public_burst_frames),
            public_burst_bytes: options
                .public_burst_bytes
                .unwrap_or(defaults.public_burst_bytes),
            public_key_budget: options.public_key_budget.or(defaults.public_key_budget),
            // `Some(0.0)` = 关掉这一层（与 `PAIR_FULL_KEY_BUDGET_BYTES=0` 同义）：
            // 容量 0 的桶会把每一帧都拦下，那不是「关掉」而是「什么都发不出去」
            full_key_budget: match options.full_key_budget {
                Some(budget) => (budget > 0.0).then_some(budget),
                None => defaults.full_key_budget,
            },
            max_sessions_per_key: options
                .max_sessions_per_key
                .unwrap_or(defaults.max_sessions_per_key),
            max_pre_handshake_connections: options
                .max_pre_handshake_connections
                .unwrap_or(defaults.max_pre_handshake_connections),
            pre_handshake_per_ip: options
                .pre_handshake_per_ip
                .unwrap_or(defaults.pre_handshake_per_ip),
            turn_secret: options.turn_secret.map(str::to_string),
            turn_ttl: options.turn_ttl.unwrap_or(defaults.turn_ttl),
            handshake_failures_per_minute: options
                .handshake_failures_per_minute
                .or(defaults.handshake_failures_per_minute),
            max_sessions: options.max_sessions,
            max_connections: defaults.max_connections,
            max_public_sessions: options.max_public_sessions,
            max_public_per_ip: options.max_public_per_ip,
            public_window: options.public_window,
            full_window: options.full_window.unwrap_or(defaults.full_window),
            stale_after: options.stale_after,
            ice_servers: options.ice_servers,
            stun_port: options.stun_port,
            // 会话层用不到密码原文（那是 `server.rs` 在升级之前判的），给固定密码的摘要
            server_keys: {
                let mut keys = vec![ServerKey::new(Tier::Full, SERVER_PASSWORD)];

                if options.public_tier {
                    keys.push(ServerKey::new(Tier::Public, PUBLIC_PASSWORD));
                }

                keys.extend(
                    options
                        .extra_keys
                        .into_iter()
                        .map(|(tier, password)| ServerKey::new(tier, password)),
                );

                keys
            },
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

        // 序号一起返回：它是公益档那把钥匙的预算归属（部署者那一档不记账，所以不影响行为）
        assert_eq!(relay.classify_server_token(&token), Some((Tier::Full, 0)));
        assert_eq!(relay.classify_server_token(""), None);
        // 密码原文不是凭据：凭据是它派生出的一串
        assert_eq!(relay.classify_server_token(SERVER_PASSWORD), None);
        assert_eq!(
            relay.classify_server_token(&auth::derive_server_token(
                "relay-unit-tests-server-password2"
            )),
            None
        );
    }

    /// 两把钥匙各归各的档：这是「谁能用多少」的唯一判据
    #[test]
    fn the_two_server_passwords_map_to_their_own_tier() {
        let open = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            public_tier: true,
            ..Options::default()
        });

        assert_eq!(
            open.classify_server_token(&auth::derive_server_token(SERVER_PASSWORD)),
            Some((Tier::Full, 0))
        );
        assert_eq!(
            open.classify_server_token(&auth::derive_server_token(PUBLIC_PASSWORD)),
            Some((Tier::Public, 1))
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
            public_tier: true,
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
            Some((Tier::Full, 0))
        );
    }

    /// 多把钥匙：同一档可以配好几把（每把给一个人，换人时只撤销一把），每把都按自己
    /// 那一档算；表里没有的钥匙一律不认（绝不能掉回任何一档）。
    #[test]
    fn every_key_carries_its_own_tier() {
        let relay = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            public_tier: true,
            extra_keys: vec![
                (Tier::Full, "relay-unit-tests-second-full-key-01"),
                (Tier::Public, "relay-unit-tests-second-public-key-1"),
            ],
            ..Options::default()
        });

        assert_eq!(relay.server_key_count(Tier::Full), 2);
        assert_eq!(relay.server_key_count(Tier::Public), 2);

        // 序号也要对：公益档的预算记在**钥匙**上，认对档位却认错钥匙等于把两把钥匙的
        // 额度混成一个。键的顺序是「完全档那一批，然后公益档那一批」
        for (password, tier, index) in [
            (SERVER_PASSWORD, Tier::Full, 0),
            (PUBLIC_PASSWORD, Tier::Public, 1),
            ("relay-unit-tests-second-full-key-01", Tier::Full, 2),
            ("relay-unit-tests-second-public-key-1", Tier::Public, 3),
        ] {
            assert_eq!(
                relay.classify_server_token(&auth::derive_server_token(password)),
                Some((tier, index)),
                "{password} 应该是 {tier:?}"
            );
        }

        assert_eq!(
            relay.classify_server_token(&auth::derive_server_token("relay-unit-tests-stranger")),
            None
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
        let key_index = key_index_for(relay, tier);
        let reservation = relay
            .reserve(room_id, auth::auth_verifier(token), tier, key_index, source)
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
        let key_index = key_index_for(relay, tier);
        let reservation = relay
            .reserve(room_id, auth::auth_verifier(token), tier, key_index, source)
            .await
            .unwrap_or_else(|rejection| panic!("预留 {room_id} 不该被拒：{rejection:?}"));

        relay.admit(reservation, device_id, sender).await
    }

    /// 这一档在**这台**服务器上的第一把钥匙是第几把。
    ///
    /// 会话层只认「摘要 + 档位」，序号是 `server.rs` 握手时算出来的（它是公益档记账的
    /// 归属）。用例里按同一个口径反推，省得每个调用点硬编码那个数——那样将来钥匙顺序一变，
    /// 一堆用例会跟着改错。
    fn key_index_for(relay: &Relay, tier: Tier) -> usize {
        relay
            .options
            .server_keys
            .iter()
            .position(|key| key.tier == tier)
            .unwrap_or_else(|| panic!("用例里没配 {tier:?} 那一档的钥匙"))
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
        let mut bucket = Bucket::new(Quota::steady(limits), start);
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

    /// [`Bucket::wait_for`] 与 [`Bucket::take`] 的判据对称：装得下就是 0，要等就报时长，
    /// 容量不够就是 `None`；而且它**只问不扣**，问过之后桶的状态一点没变。
    #[test]
    fn wait_for_is_the_mirror_of_take() {
        let start = Instant::now();
        let quota = Quota::bytes_only(1_000.0, 100.0);
        let mut bucket = Bucket::new(quota, start);

        // 满桶：现在就装得下
        assert_eq!(
            bucket.wait_for(start, 1.0, 0.0, 1_000.0),
            Some(Duration::ZERO)
        );

        assert!(bucket.take(start, 1.0, 0.0, 1_000.0));

        // 花光之后：差 1_000 字节、每秒回填 100，就是要等 10 秒
        assert_eq!(
            bucket.wait_for(start, 1.0, 0.0, 1_000.0),
            Some(Duration::from_secs(10))
        );
        // 中途问一次：回填了一半，等的时间也减半
        assert_eq!(
            bucket.wait_for(start + Duration::from_secs(5), 1.0, 0.0, 1_000.0),
            Some(Duration::from_secs(5))
        );
        // 过了一个回填窗口就补满了
        assert_eq!(
            bucket.wait_for(start + Duration::from_secs(10), 1.0, 0.0, 1_000.0),
            Some(Duration::ZERO)
        );
        // 比容量还大的一笔：等多久都装不下（`None` 唯一的来源）
        assert_eq!(bucket.wait_for(start, 1.0, 0.0, 1_001.0), None);

        // 「只问不扣」也一并钉住（`allow` 的重试就是靠它：问完再等，等到了才真扣一次，
        // 不然重试一次就多扣一笔）
        let fresh = Bucket::new(quota, start);
        let _ = fresh.wait_for(start + Duration::from_secs(5), 1.0, 0.0, 500.0);

        assert_eq!(fresh.bytes, 1_000.0, "`wait_for` 一个字节都不该扣");
        assert_eq!(
            fresh.updated_at, start,
            "`wait_for` 也不该推进 `updated_at`"
        );
    }

    /// [`WaitBudget`] 管的是**整帧**能等多久：每轮扣一次，扣不动就是「等不起」（走 4006）。
    ///
    /// 没有它的话，同一把钥匙上还有别的会话在抢额度时，被压住的这条每轮都只等一小会儿、
    /// 却会一直睡下去——既不放行也不拒绝；而且它待在 `serve` 的内层循环里，空闲回收在这
    /// 期间也碰不到它。
    ///
    /// 扣的就是**实际该睡**的时长（下限在这里抬好），所以「报出来的等待不到 1 毫秒」的那些
    /// 轮次也各扣足 1 毫秒：不然上界只钉住报出来的数字，累计睡出来的墙钟时间可以远超预算。
    #[test]
    fn a_wait_budget_bounds_the_whole_frame() {
        // 正好等于预算是允许的（说的是「最多等这么久」，不是「必须小于」）
        let mut exact = WaitBudget::new(KEY_BUDGET_WAIT_LIMIT);

        assert_eq!(
            exact.take(KEY_BUDGET_WAIT_LIMIT),
            Some(KEY_BUDGET_WAIT_LIMIT)
        );
        assert_eq!(exact.take(Duration::from_millis(1)), None, "预算已经用光");

        // 分几次扣满也一样
        let mut split = WaitBudget::new(Duration::from_secs(5));

        assert_eq!(
            split.take(Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            split.take(Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(split.take(Duration::from_secs(2)), None, "只剩 1 秒");
        assert_eq!(
            split.take(Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(split.take(Duration::ZERO), None, "用光之后连下限都扣不动");

        // 零等待也扣下限：返回值就是调用方要睡的那个数
        let mut floor = WaitBudget::new(Duration::from_millis(3));

        assert_eq!(floor.take(Duration::ZERO), Some(KEY_BUDGET_WAIT_FLOOR));
        assert_eq!(
            floor.take(Duration::from_micros(1)),
            Some(KEY_BUDGET_WAIT_FLOOR)
        );
        assert_eq!(
            floor.take(KEY_BUDGET_WAIT_FLOOR),
            Some(KEY_BUDGET_WAIT_FLOOR)
        );
        assert_eq!(floor.take(Duration::ZERO), None, "3 毫秒刚好用光");

        // 「每轮只要等几微秒」也不会把一帧拖成无穷：那些轮次各按 1 毫秒记账，
        // 所以轮数与累计睡的时间都是有界的
        let mut tiny = WaitBudget::new(Duration::from_secs(5));
        let mut slept = Duration::ZERO;
        let mut rounds = 0_u64;

        while let Some(wait) = tiny.take(Duration::from_micros(1)) {
            slept += wait;
            rounds += 1;
        }

        assert_eq!(rounds, 5_000, "每轮都得扣足下限，轮数因此有界");
        assert_eq!(slept, Duration::from_secs(5), "累计睡的时间不超过预算");
    }

    /// 「整帧等过头」这一步真的会把等待换成拒绝（4006），而不是接着睡或直接放行。
    ///
    /// 这就是读循环里 `step` 那一次的决策：单轮等待都在上限之内，但一轮轮累计过去，
    /// 预算用光的那一轮必须变成 `Refuse(Limited::KeyBudget)`。`Pass` 与别的 `Refuse` 都要
    /// 原样穿过（它们与等待无关）。
    #[test]
    fn a_spent_wait_budget_turns_a_wait_into_a_refusal() {
        let mut budget = WaitBudget::new(Duration::from_secs(5));
        let wait = Duration::from_secs(2);

        // 前两轮等得着（各自都在上限之内）
        assert_eq!(
            Allowance::Wait(wait).step(&mut budget),
            Allowance::Wait(wait)
        );
        assert_eq!(
            Allowance::Wait(wait).step(&mut budget),
            Allowance::Wait(wait)
        );

        // 只剩 1 秒，换不成「接着睡」——这一帧到此为止
        assert_eq!(
            Allowance::Wait(wait).step(&mut budget),
            Allowance::Refuse(Limited::KeyBudget),
            "预算用光之后不能再等"
        );

        // 另外两个答案不受预算影响
        assert_eq!(Allowance::Pass.step(&mut budget), Allowance::Pass);
        assert_eq!(
            Allowance::Refuse(Limited::Connection).step(&mut budget),
            Allowance::Refuse(Limited::Connection)
        );
    }

    #[test]
    fn a_twenty_chunk_burst_is_legal() {
        // 20 个 512 KiB chunk（含帧头与 nonce/tag 每个约 524 KiB）合计约 10 MiB，
        // 低于 12 MiB 的字节上限——这正是 12 MiB 这个数字的由来
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::new(Quota::steady(limits), start);
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
        let mut bucket = Bucket::new(Quota::steady(limits), start);
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
                .reserve(
                    ROOM_A,
                    auth::auth_verifier("token-wrong"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
                .reserve(
                    ROOM_C,
                    auth::auth_verifier("token-c"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
                .reserve(
                    ROOM_B,
                    auth::auth_verifier("token-b"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
                .await
                .unwrap_err(),
            RoomRejection::Capacity
        );

        relay.drop_peer(ROOM_A, "a1", a1_id).await;

        assert!(
            relay
                .reserve(
                    ROOM_B,
                    auth::auth_verifier("token-b"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
            .reserve(
                ROOM_A,
                auth::auth_verifier("token-a"),
                Tier::Full,
                key_index_for(&relay, Tier::Full),
                ip(1),
            )
            .await
            .unwrap();

        relay.release(reservation).await;

        assert!(
            relay
                .reserve(
                    ROOM_B,
                    auth::auth_verifier("token-b"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
            .reserve(
                ROOM_A,
                auth::auth_verifier("token-a"),
                Tier::Full,
                key_index_for(&relay, Tier::Full),
                ip(1),
            )
            .await
            .unwrap();
        let second = relay
            .reserve(
                ROOM_A,
                auth::auth_verifier("token-a"),
                Tier::Full,
                key_index_for(&relay, Tier::Full),
                ip(1),
            )
            .await
            .unwrap();

        relay.release(first).await;

        // 房间还在，而且仍然认同一份密钥（没有被清掉再重建）
        assert_eq!(
            relay
                .reserve(
                    ROOM_A,
                    auth::auth_verifier("wrong"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
            assert_eq!(
                relay
                    .allow(
                        id_a,
                        Tier::Full,
                        key_index_for(&relay, Tier::Full),
                        1.0,
                        0.0,
                        64.0
                    )
                    .await,
                Allowance::Pass
            );
        }
        assert_eq!(
            relay
                .allow(
                    id_a,
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    1.0,
                    0.0,
                    64.0
                )
                .await,
            Allowance::Refuse(Limited::Connection)
        );

        // 另一个 socket 有自己的桶
        assert_eq!(
            relay
                .allow(
                    id_b,
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    1.0,
                    0.0,
                    64.0
                )
                .await,
            Allowance::Pass
        );

        // 已经摘掉的连接不再有桶，也不会被 `entry().or_insert_with()` 重新造出来
        relay.drop_peer(ROOM_A, "b", id_b).await;
        assert_eq!(
            relay
                .allow(
                    id_b,
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    1.0,
                    0.0,
                    64.0
                )
                .await,
            Allowance::Refuse(Limited::Connection)
        );
    }

    /// 公益档的滚动预算记在**钥匙**上：拿同一把钥匙的会话共用一个桶，另一把钥匙不受影响。
    ///
    /// 按连接、按房间、按 IP 记账都能被绕开（房间是客户端用配对密码推出来的、一空就没了；
    /// 换配对密码就是新房间；IP 会连坐同一个 NAT，在 IPv6 上还软），只有钥匙是部署者发出去
    /// 的、拿钥匙的人换不掉。这一条把四件事一起钉住：同一把钥匙跨**会话**共用、另一把钥匙
    /// 自己那一份不动、部署者那一档完全不扣这个桶、以及**额度用完不会当场断**。
    ///
    /// 这里的额度取得极小（一次性 1000 字节），于是回填速率只有 0.28 B/s：一帧 900 字节
    /// 要等十几分钟，远超 `KEY_BUDGET_WAIT_LIMIT`。所以这一条看到的正是那个「等不起」的
    /// 分支——仍然拒。而「等一小会儿接着传」那条路要真跑出来就得把回填窗口压下来，
    /// 那是会话层单测才做得到的事（见 `a_spent_key_budget_waits_instead_of_cutting`）。
    #[tokio::test]
    async fn the_public_budget_is_per_key_not_per_room() {
        let relay = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            max_public_per_ip: 4,
            public_tier: true,
            // 第二把公益钥匙（键的顺序是「完全档那一批，然后公益档那一批」，所以它是第 3 把）
            extra_keys: vec![(Tier::Public, SECOND_PUBLIC_KEY)],
            // 预算小到一眼能数清：1000 字节一次性（回填速率 0.28 B/s，测试里约等于没有，
            // 所以「等」一定超上限——这正是这条用例要的「等不起」那一支）
            public_key_budget: Some(1000.0),
            ..Options::default()
        });
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (other_tx, _other_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (full_tx, _full_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let public = key_index_for(&relay, Tier::Public);

        // 两个**不同会话**，但都用第一把公益钥匙
        let (id_a, _, _) = join_ok_tier(&relay, ROOM_A, "token-a", "a", &a_tx, Tier::Public).await;
        let (id_b, _, _) = join_ok_tier(&relay, ROOM_B, "token-b", "b", &b_tx, Tier::Public).await;
        let (id_full, _, _) = join_ok(&relay, ROOM_C, "token-c", "c", &full_tx).await;

        assert_eq!(
            relay
                .allow(id_a, Tier::Public, public, 1.0, 0.0, 900.0)
                .await,
            Allowance::Pass
        );
        assert_eq!(
            relay
                .allow(id_b, Tier::Public, public, 1.0, 0.0, 900.0)
                .await,
            Allowance::Refuse(Limited::KeyBudget),
            "同一把钥匙跨会话共用一个桶：A 花掉之后 B 也得等（B 自己那一份还有的是），\
             而这里等不起，所以是拒绝"
        );

        // 另一把公益钥匙自己那一份没被动过：它是**另一个**会话，且换了一把钥匙
        let (id_other, _, _) =
            join_ok_tier(&relay, ROOM_A, "token-a", "a2", &other_tx, Tier::Public).await;
        let second = relay.options.server_keys.len() - 1;

        assert_eq!(
            relay
                .allow(id_other, Tier::Public, second, 1.0, 0.0, 900.0)
                .await,
            Allowance::Pass,
            "换一把钥匙就是另一份预算，不该被前一把的欠账连坐"
        );

        // 部署者那一档压根不扣这个桶：它照旧按自己的额度走
        for _ in 0..30 {
            assert_eq!(
                relay
                    .allow(
                        id_full,
                        Tier::Full,
                        key_index_for(&relay, Tier::Full),
                        1.0,
                        0.0,
                        64.0
                    )
                    .await,
                Allowance::Pass
            );
        }
    }

    /// 没转发出去的那一帧**不在钥匙上留账**。
    ///
    /// 钥匙那张表不随连接消失，所以「先扣再判、不回滚」在这里会变成会累加的欠账（每次
    /// 「重连 + 发一帧」都多记一笔）。现在这件事是**结构性**的：扣减只发生在真的要转发
    /// 那一刻（`Relay::allow` 先问 `wait_for`，两条不转发的路都不扣），没有「先扣了再还」
    /// 这回事。这一条把它钉在行为上——剩余 100 字节时，一个 900 字节的帧要等十几分钟
    /// （超过等待上限，于是被拒），而紧接着那个 50 字节的小帧仍然装得下剩下的那 100。
    #[tokio::test]
    async fn a_frame_the_budget_refuses_leaves_no_debt() {
        let relay = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            max_public_per_ip: 4,
            public_tier: true,
            public_key_budget: Some(1000.0),
            ..Options::default()
        });
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (id_a, _, _) = join_ok_tier(&relay, ROOM_A, "token-a", "a", &a_tx, Tier::Public).await;
        let public = key_index_for(&relay, Tier::Public);

        assert_eq!(
            relay
                .allow(id_a, Tier::Public, public, 1.0, 0.0, 900.0)
                .await,
            Allowance::Pass
        );
        assert_eq!(
            relay
                .allow(id_a, Tier::Public, public, 1.0, 0.0, 900.0)
                .await,
            Allowance::Refuse(Limited::KeyBudget),
            "只剩 100 字节，900 的帧要等十几分钟——等不起"
        );
        assert_eq!(
            relay
                .allow(id_a, Tier::Public, public, 1.0, 0.0, 50.0)
                .await,
            Allowance::Pass,
            "被拒的那笔根本没扣过，50 字节的帧仍然装得下剩下的 100"
        );
    }

    /// 钥匙那份额度用完了是**等到装得下**，不是断开——回填速度就是速度上限。
    ///
    /// 这一条把回填窗口压到一秒，好让「等」在测试里跑得出来：1000 字节的额度一帧就花光，
    /// 紧接着那帧要先等一小会儿才装得下，而那个时长落在 `KEY_BUDGET_WAIT_LIMIT` 之内，
    /// 因此报的是 `Allowance::Wait`（不是 `Refuse`）；真等过去之后同一帧就该放行——大文件
    /// 因此能慢慢传完，而不是被 `4006` 断在半路。最后钉住 `wait_for` 的 `None`：比整份额度
    /// 还大的一帧，等多久都不会变，只能拒绝。
    #[tokio::test]
    async fn a_spent_key_budget_waits_instead_of_cutting() {
        let relay = relay_with(Options {
            max_sessions: 20,
            max_public_sessions: 10,
            public_tier: true,
            public_key_budget: Some(1_000.0),
            // 一秒回填一整份：0.8 秒的等待因此是个毫秒级的数，用例不会真的坐等
            key_budget_window: Some(Duration::from_secs(1)),
            ..Options::default()
        });
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (id, _, _) = join_ok_tier(&relay, ROOM_A, "token-a", "a", &a_tx, Tier::Public).await;
        let public = key_index_for(&relay, Tier::Public);

        // 第一帧 900 字节：额度花得只剩 100
        assert_eq!(
            relay.allow(id, Tier::Public, public, 1.0, 0.0, 900.0).await,
            Allowance::Pass
        );

        // 第二帧差 800 字节，按每秒 1000 回填就是 0.8 秒——在等待上限之内，所以是「等」
        let wait = match relay.allow(id, Tier::Public, public, 1.0, 0.0, 900.0).await {
            Allowance::Wait(wait) => wait,
            other => panic!("额度用完该是等待（限速），不是 {other:?}"),
        };

        assert!(
            wait > Duration::ZERO && wait <= KEY_BUDGET_WAIT_LIMIT,
            "0.8 秒的等待该落在等待上限之内，实际 {wait:?}（上限 {:?}）",
            KEY_BUDGET_WAIT_LIMIT
        );

        // 等够了就装得下：这就是限速本身——慢一点，但不会被切断
        tokio::time::sleep(wait).await;

        assert_eq!(
            relay.allow(id, Tier::Public, public, 1.0, 0.0, 900.0).await,
            Allowance::Pass,
            "等过去之后同一帧就该放行"
        );

        // 一帧比整份额度还大：`wait_for` 报 `None`，等多久都没意义 → 拒
        assert_eq!(
            relay
                .allow(id, Tier::Public, public, 1.0, 0.0, 1_001.0)
                .await,
            Allowance::Refuse(Limited::KeyBudget),
            "比容量还大的一帧等不出来"
        );
    }

    /// 同一个 Room 上「已放行、还没走进 `admit`」的连接要封顶。
    ///
    /// 不封顶时这条路上**没有任何额度**：同一个 Room 能被堆上任意多条这样的连接，每条占
    /// 一个任务加一份 8 KiB 的请求头缓冲（还在 10 秒的握手超时里），而且它们的许可都算进
    /// 容量——既拖住自己，也拖住别人（`pending > 0` 的 Room 永远不被 `sweep` 清掉）。
    #[tokio::test]
    async fn a_room_refuses_to_pile_up_pending_handshakes() {
        let relay = relay(20, Duration::from_secs(120));
        let held: Vec<_> = {
            let mut held = Vec::new();

            for index in 1..=MAX_PENDING_PER_ROOM {
                let reservation = relay
                    .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, 0, ip(1))
                    .await
                    .unwrap_or_else(|rejection| panic!("第 {index} 条握手该放行：{rejection:?}"));

                held.push(reservation);
            }

            held
        };

        assert_eq!(
            relay
                .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, 0, ip(1))
                .await
                .unwrap_err(),
            RoomRejection::TooManyPending
        );

        // 还回一份之后又有位置：这正是「握手失败要还回去」走的那条路，正常重连也靠它
        let mut held = held;

        relay.release(held.pop().unwrap()).await;

        assert!(relay
            .reserve(ROOM_A, auth::auth_verifier("token-a"), Tier::Full, 0, ip(1))
            .await
            .is_ok());
    }

    /// 握手失败按 IP 限速，而且**同一份计数**决定日志写不写。
    ///
    /// 拿错密码刷 `/ws` 的代价本来只是一次摘要比较，能被打爆的是**日志**（部署者排障的
    /// 唯一证据，被刷掉就等于没有）。这一条钉住三个形状：没失败过的 IP 直接放行、失败
    /// 攒够了就被挡下、被挡下之后不再写日志——而别的 IP 一点不受影响。
    #[tokio::test]
    async fn a_flood_of_failed_handshakes_is_throttled_per_ip() {
        let relay = relay(20, Duration::from_secs(120));

        // 还没失败过：连桶都没建，直接放行
        assert!(relay.handshake_allowed(ip(1)).await);

        // 前 30 次都该写日志（也就是都返回 true）
        for index in 1..=(DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE as usize) {
            assert!(
                relay.note_handshake_failure(ip(1)).await,
                "第 {index} 行日志该照写"
            );
        }

        assert!(
            !relay.note_handshake_failure(ip(1)).await,
            "额度用光之后这一行该静默（连接照样按状态码被拒）"
        );
        assert!(
            !relay.handshake_allowed(ip(1)).await,
            "而且下一次握手该被 429"
        );

        // 另一个 IP 有自己的额度：刷爆一个不该连坐别人
        assert!(relay.handshake_allowed(ip(2)).await);
        assert!(relay.note_handshake_failure(ip(2)).await);
    }

    /// 封禁时长有个上界：刷得再久也只是「继续被封」，不是「刷多久封多久」。
    ///
    /// 欠账是拿「还差多少额度」算的，没有下界就会一直涨——刷五分钟错密码能封一个 IP
    /// 八个多小时，同一个 NAT（或 IPv6 的 /64）后面的人跟着连坐。这里钉住「桶底 = 一份
    /// 满额度」：两千次失败之后欠账仍然停在下界上。
    #[tokio::test]
    async fn a_long_flood_cannot_extend_the_ban_forever() {
        let relay = relay(20, Duration::from_secs(120));
        let limit = DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE;

        for _ in 0..2_000 {
            relay.note_handshake_failure(ip(1)).await;
        }

        assert!(!relay.handshake_allowed(ip(1)).await);

        let state = relay.state.lock().await;
        let bucket = state.handshake_failures.get(&ip(1)).unwrap();

        assert!(bucket.frames >= -limit, "欠账越过下界了：{}", bucket.frames);
        // 回填到「能再握手」（`frames >= 1`）要 (limit + 1) / (limit / 60) ≈ 62 秒，
        // 与刷了多少次无关
        assert!(bucket.frames <= -limit + 1.0);
    }

    /// 清理只在**要插一个新地址**时做：表停在高位时，老地址反复失败不该每次都扫全表。
    #[tokio::test]
    async fn the_failure_table_is_only_swept_for_a_new_address() {
        let relay = relay(20, Duration::from_secs(120));
        let limit = DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE;
        let quota = Quota::frames_only(limit, limit / 60.0);

        // 「额度已经回满」的条目：正是清理该扔掉的那一种
        let idle = || Bucket::new(quota, Instant::now());

        {
            let mut state = relay.state.lock().await;

            for index in 0..HANDSHAKE_FAILURE_TABLE_SWEEP {
                let address = IpAddr::from([10, 1, (index >> 8) as u8, index as u8]);

                state
                    .handshake_failures
                    .insert(IpKey::from_addr(address), idle());
            }

            // 一条早就安静下来的旧账（表顶到阈值之后才插的，所以它一定还在）
            state.handshake_failures.insert(ip(250), idle());
            // 这个地址接着还要再失败一次
            state.handshake_failures.insert(ip(251), idle());
        }

        // 老地址又失败一次：不插新条目，也就不扫表——旧账还躺着
        assert!(relay.note_handshake_failure(ip(251)).await);
        assert!(relay
            .state
            .lock()
            .await
            .handshake_failures
            .contains_key(&ip(250)));

        // 新地址失败：这才是「表到顶了」的时刻，顺手清一遍
        assert!(relay.note_handshake_failure(ip(252)).await);

        let state = relay.state.lock().await;

        // 该清的清掉（已经回满的旧账）……
        assert!(!state.handshake_failures.contains_key(&ip(250)));
        // ……而**还欠着**的条目一条都不能少：把 `retain` 的判据放宽（比如 `<= limit`）
        // 会让表被反复清空、桶被重建，限速整条废掉。这一条就是钉住它的。
        assert!(state.handshake_failures.contains_key(&ip(251)));
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
            public_tier: true,
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
        join_as(&relay, ROOM_B, "token-b", "b1", &full_tx, Tier::Full, ip(1)).await;

        // 于是两档各拒各的：两边都是「自己那一档满了」
        assert_eq!(
            relay
                .reserve(
                    &room_id_of('c'),
                    auth::auth_verifier("token-c"),
                    Tier::Public,
                    key_index_for(&relay, Tier::Public),
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
                    key_index_for(&relay, Tier::Full),
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
                .reserve(
                    ROOM_A,
                    auth::auth_verifier("token-a"),
                    Tier::Full,
                    key_index_for(&relay, Tier::Full),
                    ip(1)
                )
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
            Admit::Accepted {
                peer_online: true,
                ..
            }
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
                .reserve(
                    ROOM_B,
                    auth::auth_verifier("token-b"),
                    Tier::Public,
                    key_index_for(&relay, Tier::Public),
                    ip(1)
                )
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
        match join_as(relay, room_id, token, device_id, sender, source_tier, ip(1)).await {
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
        relay.state.lock().await.rooms.keys().cloned().collect()
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
            client_ip(private_peer, &["198.51.100.7"], false),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );

        // 开了开关 + 私网 peer：取**最后一个**（可信那一跳自己追加的）
        assert_eq!(
            client_ip(private_peer, &["198.51.100.7, 203.0.113.9"], true),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );

        // 同一个头被拆成**多行**时也要取整体最后一项：Go 的 `net/http` 给一个已经存在的头
        // append 值就是写成另一行，只看第一行等于让客户端伪造的那一行赢
        assert_eq!(
            client_ip(private_peer, &["198.51.100.7", "203.0.113.9"], true),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );
        // 多行 + 空行 + 两侧空格：一样只看最后那个非空项
        assert_eq!(
            client_ip(
                private_peer,
                &["198.51.100.7, 192.0.2.1", "", " 203.0.113.9 "],
                true
            ),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );
        // 最后一项本身就是空（某一跳留了个尾逗号）：退回 peer，**不能**往前退到客户端写的
        // 那一项上——那等于把伪造值又放回来
        assert_eq!(
            client_ip(private_peer, &["198.51.100.7", "203.0.113.9,"], true),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );

        // 公网 peer 伪造 XFF：整条头都不看
        assert_eq!(
            client_ip(public_peer, &["198.51.100.7"], true),
            IpKey::from_addr("203.0.113.9".parse().unwrap())
        );

        // 畸形 / 缺失：退回 peer，绝不能因此少算一个 IP（限额会漏）
        assert_eq!(
            client_ip(private_peer, &["not-an-ip"], true),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );
        assert_eq!(
            client_ip(private_peer, &[], true),
            IpKey::from_addr("172.18.0.5".parse().unwrap())
        );
    }

    /// 预握手闸：宽、有硬上界，而且两份计数（总量 + 每个来源）都在 `Drop` 里还回去。
    ///
    /// 它管的是「读请求头那一段」，所以**不能**是真实额度那道闸（见 `handle` 里的注释）；
    /// 这一条把它的三条性质都钉住：每个来源单独封顶、总量封顶、归还之后表要自己缩回去。
    #[test]
    fn the_pre_handshake_gate_is_bounded_and_gives_everything_back() {
        let relay = relay_with(Options {
            max_sessions: 20,
            // 用例只关心形状：3 个总量、2 个每 IP（真实部署里分别是 512 与 64）
            max_pre_handshake_connections: Some(3),
            pre_handshake_per_ip: Some(2),
            ..Options::default()
        });
        let first: IpAddr = "203.0.113.7".parse().unwrap();
        let second: IpAddr = "203.0.113.8".parse().unwrap();

        let a = relay.try_pre_handshake_permit(first).expect("第 1 条");
        let b = relay.try_pre_handshake_permit(first).expect("第 2 条");

        // 同一个来源别想把池子吃光
        assert!(relay.try_pre_handshake_permit(first).is_none());

        // 总量还剩 1：另一个来源照进
        let c = relay.try_pre_handshake_permit(second).expect("另一个来源");

        assert!(relay.try_pre_handshake_permit(second).is_none(), "总量满了");

        // 还一个：总量与那个来源的计数一起还
        drop(a);
        assert!(
            relay.try_pre_handshake_permit(first).is_some(),
            "还回去之后要能再进"
        );

        drop(b);
        drop(c);

        // 两份计数都归零，而且表不能留着一堆零值的条目（扫描器会带来一大堆只用一次的地址）
        assert!(
            relay.pre_handshake_per_ip.lock().unwrap().is_empty(),
            "计数归零要删键"
        );

        // `0` = 不按来源限（只留总量那一道）
        let unlimited = relay_with(Options {
            max_sessions: 20,
            max_pre_handshake_connections: Some(3),
            pre_handshake_per_ip: Some(0),
            ..Options::default()
        });

        // 三张都**留住**（临时值会在语句末尾就归还，那样测的就不是这道闸了）
        let held: Vec<_> = (0..3)
            .map(|_| unlimited.try_pre_handshake_permit(first).expect("总量还够"))
            .collect();

        assert!(
            unlimited.try_pre_handshake_permit(first).is_none(),
            "总量仍然要封顶"
        );

        drop(held);
    }

    /// 同一把钥匙最多开几组会话（`PAIR_MAX_SESSIONS_PER_KEY`）。
    ///
    /// 它挡的是「一把流出去的钥匙把整档名额吃光」，而不是「同一个会话的第二个人」：同一个
    /// Room 的第二条连接走的是另一条分支，数出来的仍然只是一组。
    #[tokio::test]
    async fn one_key_can_only_open_so_many_sessions() {
        let relay = relay_with(Options {
            max_sessions: 20,
            max_sessions_per_key: Some(2),
            ..Options::default()
        });
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (c_tx, _c_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let full = key_index_for(&relay, Tier::Full);

        join_as(&relay, ROOM_A, "token-a", "a1", &a_tx, Tier::Full, ip(1)).await;
        // 同一个会话的第二个人：不受这把钥匙的会话数影响
        join_as(&relay, ROOM_A, "token-a", "a2", &b_tx, Tier::Full, ip(1)).await;
        // 第 2 组：正好用满
        join_as(&relay, ROOM_B, "token-b", "b1", &c_tx, Tier::Full, ip(1)).await;

        assert_eq!(
            relay
                .reserve(
                    ROOM_C,
                    auth::auth_verifier("token-c"),
                    Tier::Full,
                    full,
                    ip(1)
                )
                .await
                .unwrap_err(),
            RoomRejection::KeySessionLimit
        );
    }

    /// 完全档也有「每把钥匙的滚动预算」，而且与公益档**各记各的**（桶按钥匙序号分开）。
    #[tokio::test]
    async fn the_full_tier_has_its_own_key_budget() {
        const SECOND_FULL_KEY: &str = "relay-unit-tests-second-full-key-01";

        let relay = relay_with(Options {
            max_sessions: 20,
            full_key_budget: Some(1000.0),
            extra_keys: vec![(Tier::Full, SECOND_FULL_KEY)],
            ..Options::default()
        });
        let (a_tx, _a_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (id, _, _) = join_ok(&relay, ROOM_A, "token-a", "a", &a_tx).await;
        let first = key_index_for(&relay, Tier::Full);
        let second = relay
            .options
            .server_keys
            .iter()
            .position(|key| key.verifier == crate::auth::server_verifier(SECOND_FULL_KEY))
            .expect("第二把完全钥匙");

        // 1000 字节的预算：一帧 900 字节过，再来一帧就要等十几分钟（回填 0.28 B/s），
        // 超过等待上限，于是被拒
        assert_eq!(
            relay.allow(id, Tier::Full, first, 1.0, 0.0, 900.0).await,
            Allowance::Pass
        );
        assert_eq!(
            relay.allow(id, Tier::Full, first, 1.0, 0.0, 900.0).await,
            Allowance::Refuse(Limited::KeyBudget)
        );

        // 另一把钥匙有自己的桶（「一把钥匙一个人」的另一面：额度不共享）
        assert_eq!(
            relay.allow(id, Tier::Full, second, 1.0, 0.0, 900.0).await,
            Allowance::Pass
        );

        // `0` = 不设这一层：回到「只按连接自己的额度算」
        let unlimited = relay_with(Options {
            max_sessions: 20,
            full_key_budget: Some(0.0),
            ..Options::default()
        });
        let (b_tx, _b_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (b_id, _, _) = join_ok(&unlimited, ROOM_A, "token-a", "a", &b_tx).await;

        assert_eq!(
            unlimited
                .allow(b_id, Tier::Full, first, 1.0, 0.0, 900.0)
                .await,
            Allowance::Pass
        );
    }

    /// 两档各有自己的空闲窗口：回收时按档位取（公益档默认 180 秒、完全档默认 300 秒）
    #[test]
    fn each_tier_has_its_own_idle_window() {
        let relay = relay_with(Options {
            max_sessions: 20,
            public_window: Some(Duration::from_secs(180)),
            full_window: Some(Some(Duration::from_secs(300))),
            ..Options::default()
        });

        assert_eq!(relay.window_for(Tier::Full), Some(Duration::from_secs(300)));
        assert_eq!(
            relay.window_for(Tier::Public),
            Some(Duration::from_secs(180))
        );

        // `0`（也就是 `None`）= 不回收，两档各算各的
        let off = relay_with(Options {
            max_sessions: 20,
            public_window: None,
            full_window: Some(None),
            ..Options::default()
        });

        assert_eq!(off.window_for(Tier::Full), None);
        assert_eq!(off.window_for(Tier::Public), None);
    }

    /// 限时 TURN 凭据：`base64(HMAC-SHA1(secret, "<过期秒>:<标识>"))`。
    ///
    /// 算法本身用 RFC 2202 的向量钉住——签错了在部署侧只表现为「打洞一直失败」，和凭据这件
    /// 事完全联系不起来，所以这里必须钉在标准向量上，而不是钉在「我自己算的另一遍」上。
    #[test]
    fn the_turn_credential_matches_rfc_2202() {
        // 向量 1：key = 20 个 0x0b，data = "Hi There"
        assert_eq!(
            turn_credential("\u{b}".repeat(20).as_str(), "Hi There").unwrap(),
            "thcxhlUFcmTii8C2+zeMjvFGvgA="
        );

        // 向量 2：key = "Jefe"
        assert_eq!(
            turn_credential("Jefe", "what do ya want for nothing?").unwrap(),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
    }

    /// 只给 `turn:` 条目现签凭据：`stun:` 条目原样留着（STUN 不鉴权，给它加凭据只会让一份
    /// 干净的探针配置变得看不懂）。
    #[test]
    fn signing_turn_credentials_leaves_stun_entries_alone() {
        let servers = serde_json::json!([
            { "urls": ["stun:cat.example.com:3478"] },
            {
                "urls": ["turn:cat.example.com:3478"],
                "username": "static-user",
                "credential": "static-pass"
            },
            { "urls": ["turns:cat.example.com:5349?transport=tcp"] }
        ]);
        let signed =
            sign_turn_credentials(&servers, "shared-secret", Duration::from_secs(3600), 0).unwrap();
        let entries = signed.as_array().unwrap();

        assert_eq!(
            entries[0],
            serde_json::json!({ "urls": ["stun:cat.example.com:3478"] })
        );

        for entry in &entries[1..] {
            let username = entry["username"].as_str().unwrap();
            let (expire, identity) = username.split_once(':').expect("形状要是 {过期}:{标识}");

            assert_eq!(identity, "1", "标识是第几把钥匙（1 起）");
            assert_eq!(
                entry["credential"].as_str().unwrap(),
                turn_credential("shared-secret", username).unwrap()
            );

            let expire: u64 = expire.parse().unwrap();
            let now = unix_now();

            assert!(
                expire > now + 3000 && expire <= now + 3600,
                "过期时刻要落在 TTL 里，实际 {expire}"
            );
        }

        // 第 2 把钥匙签出来的标识不一样（同一份密钥、同一份清单，但看得出是谁在用）
        let second =
            sign_turn_credentials(&servers, "shared-secret", Duration::from_secs(3600), 1).unwrap();

        assert!(second[1]["username"].as_str().unwrap().ends_with(":2"));
    }

    /// 配了共享密钥之后，welcome 里那份**静态**凭据被换成限时凭据；公益档仍然只拿 `stun:`。
    #[test]
    fn a_turn_secret_replaces_the_static_credential() {
        let servers = serde_json::json!([
            { "urls": ["stun:cat.example.com:3478"] },
            {
                "urls": ["turn:cat.example.com:3478"],
                "username": "static-user",
                "credential": "static-pass"
            }
        ]);
        let relay = relay_with(Options {
            max_sessions: 20,
            ice_servers: Some(servers),
            turn_secret: Some("unit-test-turn-shared-secret"),
            public_tier: true,
            ..Options::default()
        });
        let advertisement = relay
            .ice_servers_for(Some("cat.example.com:8080"), Tier::Full, 0)
            .unwrap();

        assert_eq!(
            advertisement[0],
            serde_json::json!({ "urls": ["stun:cat.example.com:3478"] })
        );
        assert_ne!(advertisement[1]["credential"], "static-pass");
        assert_eq!(
            advertisement[1]["credential"].as_str().unwrap(),
            turn_credential(
                "unit-test-turn-shared-secret",
                advertisement[1]["username"].as_str().unwrap()
            )
            .unwrap()
        );

        // 公益档那一条路不变：限时凭据也是 `turn:`，一样不给
        assert_eq!(
            relay.ice_servers_for(Some("cat.example.com:8080"), Tier::Public, 0),
            Some(serde_json::json!([{ "urls": ["stun:cat.example.com:3478"] }]))
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
            public_tier: true,
            ..Options::default()
        });

        assert_eq!(
            relay.ice_servers_for(Some("cat.example.com:8080"), Tier::Full, 0),
            Some(servers)
        );
        assert_eq!(
            relay.ice_servers_for(Some("cat.example.com:8080"), Tier::Public, 0),
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
            public_tier: true,
            ..Options::default()
        });

        assert!(turn_only
            .ice_servers_for(Some("cat.example.com:8080"), Tier::Public, 0)
            .is_none());

        // 内置 STUN 那一条路两档一样（STUN 本来就不鉴权）
        let builtin = relay_with(Options {
            max_sessions: 20,
            stun_port: Some(3479),
            public_tier: true,
            ..Options::default()
        });

        assert_eq!(
            builtin.ice_servers_for(Some("cat.example.com:8080"), Tier::Public, 0),
            Some(serde_json::json!([{ "urls": ["stun:cat.example.com:3479"] }]))
        );
    }

    /// 公益档的桶是**另一份形状**：突发与持续分成两个数，而且都比部署者那一档小。
    ///
    /// 只看「额度」那一项已经不够了——决定诚实打洞能不能一口气发完的是**突发容量**，
    /// 决定「拿信令帧夹带数据」能跑多快的是**回填速率**。这一条把两半都钉住，顺带钉住
    /// 完全档仍是「容量 = 速率」（那是与 Cloudflare 版逐条一致的地方，不能顺手改成两半）。
    #[test]
    fn the_public_tier_uses_its_own_limits() {
        let relay = public_relay(20, 20, 0);
        let start = Instant::now();
        let public = Bucket::new(relay.connection_quota(Tier::Public), start);
        let full = Bucket::new(relay.connection_quota(Tier::Full), start);
        let public_limits = relay.limits_for(Tier::Public);

        assert_eq!(
            public.frames,
            protocol::DEFAULT_PUBLIC_BURST_FRAMES,
            "突发给足：一轮 12 帧 / 8 KB 要能一口气发完"
        );
        assert_eq!(public.bytes, protocol::DEFAULT_PUBLIC_BURST_BYTES);
        assert!(public.frames > protocol::DEFAULT_PUBLIC_MAX_FRAMES_PER_SECOND);

        // 持续那一半就是广告出去的那份额度
        assert_eq!(public.quota.frames_rate, public_limits.frames_per_second);
        assert_eq!(public.quota.bytes_rate, public_limits.bytes_per_second);
        assert!(
            public.quota.bytes_rate < full.quota.bytes_rate,
            "公益档的持续额度必须比部署者那一档小"
        );

        // 完全档：容量 = 速率（与 CF 版逐条一致）
        assert_eq!(full.frames, full.quota.frames_rate);
        assert_eq!(full.bytes, full.quota.bytes_rate);
    }
}

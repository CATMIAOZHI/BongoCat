//! P2P（WebRTC）传输。
//!
//! 只在 Windows 上编译（`mod.rs` 里的 `#[cfg(windows)]`）：Phase 8 的范围就是
//! Windows 客户端，非 Windows 的 release 目标连 `webrtc` 那棵依赖树都不编译。
//!
//! 这一层只负责**传输本身**——`PeerConnection` 生命周期、ICE 配置、DataChannel。
//! 信令（`pair.signal`）走中继，归 `manager.rs` 管：它把对端信令交给
//! [`P2pLink::handle_signal`]，要发出去的信令与状态变化从 [`P2pLink::next_event`]
//! 取回。这一层既不认识 `PairManager`，也不碰 UI。
//!
//! 设计见 `docs/pair-plan-cloud-p2p.md` 的 §4 / §5 与修订记录 R21 / R28。两条硬约束：
//!
//! - **对 webrtc 的 `Result` 一律不许 `unwrap` / `expect`**：release profile 是
//!   `panic = "abort"`，一次 panic 会把整个 App 带走。所有失败都变成
//!   [`P2pEvent::ChannelClosed`] 或直接丢掉。
//! - **ICE 绑定绝不能是回环**：这里显式绑通配地址（`0.0.0.0:0`，也是 crate 自己的
//!   示例写法），绑回环就收不到真实 host 候选（R24-1 的 spike 只覆盖了回环）。
//!
//! 一轮协商的形状：双方各自发 `hello` → **deviceId 字典序小的一方发起 offer**
//! （避免双方同时 offer 的 glare）→ 交换 SDP 与 ICE candidate → `pet-state` 通道打开。
//! 失败就隔 [`RETRY_DELAY`] 重来；收到对端新的 `hello`（说明对端重连了）立刻重开一轮。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use tauri_plugin_log::log::{info, warn};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceConnectionState, RTCIceGatheringState, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, SettingEngineBuilder,
};

use super::link::Lane;
use super::manual::{CODE_GATHER_TIMEOUT, ManualCodeKind};
use super::protocol::{FEATURE_RELIABLE_CHANNEL, IceServer, PairSignalPayload, SIGNAL_VERSION};

/// 只跑可覆盖流（宠物快照、统计）的通道（§5.2）：`ordered = false, maxRetransmits = 0`。
/// 附件分片绝不能走这里——它要求分片序号严格递增，而这条通道会丢帧（R21）。
const PET_STATE_LABEL: &str = "pet-state";

/// 有序可靠的第二条通道（§5.2 / Phase 10）：聊天、暂离、控制与附件分片走它。
///
/// **能力门控**（R32）：只有对端在 `hello` 里声明了 [`FEATURE_RELIABLE_CHANNEL`] 才会
/// 建 / 认领它。老客户端的 `on_data_channel` 是无条件认领任何通道的，多给它一条通道
/// 会让它 last-wins 抢走 `pet-state` 那条的出站方向。
const RELIABLE_LABEL: &str = "reliable";

/// DC 的发送缓冲上限（R32）。不设上限时 `send` 永不阻塞，慢链路下就是内存无界增长。
///
/// 取值按「几个分片」定而不是照抄 crate 文档建议的 16 MiB：那段建议是给「只跑批量数据」
/// 的通道写的，而 `reliable` 是三用的（聊天 / 控制 / 分片），排队量直接等于聊天的队头
/// 延迟。192 KiB 正好是 4 个 48 KiB 的分片。
const SEND_BUFFER_LIMIT: usize = 192 * 1024;
/// 高水位 = 上限本身：一越过上限就让「还能不能发」翻假，源头停止注入。
const SEND_BUFFER_HIGH: u32 = (SEND_BUFFER_LIMIT) as u32;
/// 低水位 = 一个分片：排空到只剩一块时重新放行。
const SEND_BUFFER_LOW: u32 = 48 * 1024;

/// 一轮协商失败后，发起方隔多久再试一次（第一次）。之后每失败一次翻倍，封顶
/// [`RETRY_DELAY_MAX`]：双方都在 NAT 后面又没配 STUN 时永远打不通，固定 5 秒会让
/// 两边一直重建 `PeerConnection`、白白占 CPU 和端口。
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// 退避的上限：网络恢复后最多等这么久就会再试（界面上没有手动重试入口）
const RETRY_DELAY_MAX: Duration = Duration::from_secs(120);

/// 手工码模式下 ICE 的「多久没通就算失败」。
///
/// 默认是 `rtc-ice` 的 disconnected 5 秒 + failed 25 秒 = **30 秒**，而粘贴方的计时是从
/// 它**生成回码**那一刻开始算的——回码要靠人转送给对方（微信 / QQ），30 秒太紧，对方慢
/// 一点整次配对就作废。这里放宽到 2 分半，代价只是「真打不通时多等一会儿」。
///
/// **只放宽手工码这一轮的 PC**：中继模式的 P2P 是纯加速腿，失败判定越快越好（快失败才能
/// 快退避重试），绝不能跟着变慢。
const MANUAL_DISCONNECTED_TIMEOUT: Duration = Duration::from_secs(10);
const MANUAL_FAILED_TIMEOUT: Duration = Duration::from_secs(120);

/// 连续失败 `failures` 次之后该等多久：5 秒起、每次翻倍、封顶 2 分钟
fn retry_delay(failures: u32) -> Duration {
    RETRY_DELAY
        .saturating_mul(1u32 << failures.min(10))
        .min(RETRY_DELAY_MAX)
}

/// 这段（JSON 形态的）SDP 里带了几条候选，其中几条不是 `host`。返回（总数，非 host 数）。
///
/// 手工码模式用它区分「跨网络大概能连」与「只有本机可达」：候选一条都没有时码是废的
/// （调用方不出码、直接报错）；而**全是 `host`** 时跨网络一定连不上——`srflx` 才是 STUN
/// 帮我们要到的外网映射。
///
/// 这里数「非 host」而不是只看总数：多网卡（有线 + 无线 + 虚拟网卡 / VPN）的机器上没有
/// STUN 也会有 2~3 条 host 候选，只看总数会把一段跨网络连不上的码判成正常。
fn candidate_stats(description: &str) -> (usize, usize) {
    let Ok(parsed) = serde_json::from_str::<RTCSessionDescription>(description) else {
        return (0, 0);
    };

    let mut total = 0;
    let mut non_host = 0;

    for line in parsed
        .sdp
        .lines()
        .filter(|line| line.starts_with("a=candidate"))
    {
        total += 1;

        // `a=candidate:... typ host` / `typ srflx`：`typ` 后面那个词就是类型
        let kind = line.split_whitespace().skip_while(|word| *word != "typ").nth(1);

        // 认不出来的行按 host 算：宁可少报「能连」，不要给出一段其实连不上的码
        if matches!(kind, Some(kind) if kind != "host") {
            non_host += 1;
        }
    }

    (total, non_host)
}

/// 协商现场写进日志时用的 ICE 服务器描述：只写地址，凭据一律不进日志。
///
/// `turn:user:pass@host` 这种把凭据塞进 URL 的写法也要挡住，所以 '@' 之前的部分全部丢掉。
fn describe_ice_servers(servers: &[IceServer]) -> String {
    let urls: Vec<&str> = servers
        .iter()
        .flat_map(|server| server.urls.iter())
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .collect();

    if urls.is_empty() {
        return "没有（只有 host 候选）".to_string();
    }

    urls.iter()
        .map(|url| url.rsplit('@').next().unwrap_or(url))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 候选的日志描述：类型 + 协议 + 地址。
///
/// 判断「直连是真的直连、还是绕了 TURN 中转」要看候选里有没有 `relay`，光靠「已直连」
/// 那个徽标看不出来。
fn describe_candidate(candidate: &str) -> String {
    if candidate.trim().is_empty() {
        return "候选收集结束".to_string();
    }

    let tokens = candidate.split_whitespace();
    let proto = tokens.clone().nth(2).unwrap_or("?");
    let address = tokens.clone().nth(4).unwrap_or("?");
    let typ = tokens
        .skip_while(|token| *token != "typ")
        .nth(1)
        .unwrap_or("unknown");

    format!("{typ} {proto} {address}")
}

/// 驱动循环的输入。PC 回调、会话层交给它的对端信令、会话层要发出去的字节，都走这一条
/// 通道；驱动循环是唯一会向 [`P2pEvent`] 里写东西的地方（`Leg` 只发信令）。
enum Input {
    /// 中继上收到的**对端**信令，交给这一轮协商
    Peer(PairSignalPayload),
    /// 本层要送出去的信号（PC 回调产生的 candidate）
    Signal(PairSignalPayload),
    /// 手工码模式：不等 `hello` 就直接开一轮（角色与能力位都随码带过来）
    Begin {
        offerer: bool,
        features: Vec<String>,
    },
    /// 本端的候选已经收集完了（ICE gathering = `Complete`）
    GatheringComplete,
    /// DataChannel 收到的字节
    Inbound(Lane, Vec<u8>),
    /// 在指定的 DataChannel 上发一帧
    Outbound(Lane, Vec<u8>),
    /// 远端开了一条通道（`on_data_channel`），交给驱动循环接管
    Adopt(Arc<dyn DataChannel>),
    /// DataChannel 开了
    Ready(Lane),
    /// 这一轮废了
    Failed,
    /// 会话结束，收摊
    Stop,
}

/// 这条腿发回给会话层的事件。
#[derive(Debug)]
pub enum P2pEvent {
    /// 要经中继送出去的信令
    Signal(PairSignalPayload),
    /// 手工码模式：这一轮的本地描述（含已收集候选）已经可以打成一串码了。
    /// 与 `Signal(Offer/Answer)` 互斥——手工模式不发那两类信令。
    Gathered {
        kind: ManualCodeKind,
        /// 这段描述里带了几条候选。0 = 只有本机可达（调用方要如实告诉用户）
        candidates: usize,
        /// 其中几条不是 `host`（`srflx` / `relay`）。0 = 全是本机地址，跨网络连不上
        non_host: usize,
        description: String,
    },
    /// 收到对端的 hello，开始协商（UI 用；对端不支持 P2P 时永远收不到）
    Negotiating,
    /// 某条 DataChannel 收到的原始字节，交给 `manager.rs` 的 `handle_binary`
    Inbound(Lane, Vec<u8>),
    /// 某条 DataChannel 可以用了：探针开始（Phase 8c 起也是「可以把这条腿上的流
    /// 切过来」的信号）
    ChannelOpen(Lane),
    /// 某条 DataChannel 不能用了。**重复收到是无害的**（没开过也会收到，比如 ICE 直接
    /// 失败），调用方按「现在不可用」处理即可。
    ChannelClosed(Lane),
    /// 这一轮协商失败了（ICE 打不通 / 通道断了），正在按退避等下一轮。UI 用它把
    /// 「正在建立直连」换成「暂时连不上、已走服务器」；重复收到无害。
    Unreachable,
}

/// 一条 P2P 腿。会话层持有它；丢掉它即收摊（见 `Drop`）。
///
/// 事件流是**分开**的对象（[`P2pEvents`]）：`live` 的 `select!` 要一边用 `&mut` 轮询
/// 事件、一边用 `&` 发信令，同一个值没法同时被可变借用两次。
pub struct P2pLink {
    input: mpsc::UnboundedSender<Input>,
    /// 可靠那条通道现在还能不能收下一帧（发送缓冲的高 / 低水位事件在维护它）。
    ///
    /// 放在这里而不是让调用方去 `await DataChannel::writable()`：后者是「等到有空间为止」
    /// 的异步原语，在 `live` 的 `select!` 分支里 await 会把中继腿的入站读取一起挡住。
    writable: Arc<AtomicBool>,
}

/// 这条腿发回来的事件流。
pub struct P2pEvents {
    events: mpsc::UnboundedReceiver<P2pEvent>,
}

impl P2pEvents {
    /// 取下一条事件；`None` 表示这条腿结束了（调用方应当停止轮询这一支）
    pub async fn next(&mut self) -> Option<P2pEvent> {
        self.events.recv().await
    }
}

impl P2pLink {
    /// 起一条腿，并立刻向对端宣告自己支持 P2P（R21：能力门控只看对端）。
    ///
    /// `ice_servers` 来自 `server.welcome` 的广告（R21）：自建中继默认给它内置的 STUN；
    /// 空表示只有 host candidate（Cloudflare 版不广告任何东西）。手工码模式没有中继，
    /// 传进来的是用户在设置里自己填的那份公益 STUN（见 `manual.rs`）。
    pub fn spawn(device_id: String, ice_servers: Vec<IceServer>) -> (Self, P2pEvents) {
        let (input, incoming) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let writable = Arc::new(AtomicBool::new(true));

        // 驱动循环自己也要往 `Input` 里发（接管远端通道时要起泵），所以留一份发送端
        tokio::spawn(drive(
            device_id,
            ice_servers,
            input.clone(),
            incoming,
            events,
            Arc::clone(&writable),
        ));

        (Self { input, writable }, P2pEvents { events: event_rx })
    }

    /// 把中继上收到的对端信令交给这条腿。腿已经收摊就丢掉——P2P 是尽力而为的加速层，
    /// 丢了不影响中继上的功能。
    pub fn handle_signal(&self, signal: PairSignalPayload) {
        let _ = self.input.send(Input::Peer(signal));
    }

    /// 手工码模式的开场：不等 `hello` 直接开一轮，角色与能力位都由码决定。
    ///
    /// `features` 必须带上对方码里的能力位：能力门控只看对端（R32），缺了它
    /// `acceptable_lane("reliable")` 会返回 `None`，聊天 / 附件 / 语音就全废。
    pub fn begin(&self, offerer: bool, features: Vec<String>) {
        let _ = self.input.send(Input::Begin { offerer, features });
    }

    /// 在指定的 DataChannel 上发一帧。通道没开就丢掉（同上，尽力而为）。
    pub fn send(&self, lane: Lane, frame: Vec<u8>) {
        let _ = self.input.send(Input::Outbound(lane, frame));
    }

    /// 可靠那条通道现在还能不能收下一帧（同步的布尔读，见字段注释）。
    pub fn writable(&self) -> bool {
        self.writable.load(Ordering::Relaxed)
    }
}

impl Drop for P2pLink {
    fn drop(&mut self) {
        // 会话结束：让驱动循环收摊，把 PeerConnection 关掉。不这么做的话 PC 回调与
        // DataChannel 泵都还握着 input 的发送端，驱动循环会一直挂着。
        let _ = self.input.send(Input::Stop);
    }
}

/// 驱动循环：串起「收到信令 → 推进协商 → 发出信令」与失败重试。
async fn drive(
    device_id: String,
    ice_servers: Vec<IceServer>,
    driver: mpsc::UnboundedSender<Input>,
    mut incoming: mpsc::UnboundedReceiver<Input>,
    events: mpsc::UnboundedSender<P2pEvent>,
    writable: Arc<AtomicBool>,
) {
    let mut leg = Leg {
        device_id,
        ice_servers,
        driver,
        events,
        peer_ready: false,
        offerer: false,
        peer: None,
        pet_state: None,
        reliable: None,
        peer_features: Vec::new(),
        manual: false,
        gathering_complete: false,
        code_pending: None,
        code_sent: false,
        code_deadline: None,
        writable,
        remote_ready: false,
        buffered_candidates: Vec::new(),
    };
    let mut retry_at: Option<tokio::time::Instant> = None;
    // 连续失败了几轮：决定下一轮等多久。通道开了、或对端重新 hello 时清零。
    let mut failures: u32 = 0;

    leg.announce();

    // `leg.driver` 与循环里的 `incoming` 是同一个通道的两端：`Leg` 用它把失败送回
    // 来，循环用它接管远端通道时起泵。
    let driver = leg.driver.clone();

    loop {
        let retry = async {
            match retry_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        // 手工码模式「等候选收集完成」的兜底（见 `Leg::emit_code`）：`Complete` 可能永远
        // 不来。先把时限拷出来，`select!` 里才不用同时借 `leg`（另一支要可变借它）
        let code_wait_at = leg.code_deadline;
        let code_wait = async move {
            match code_wait_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            next = incoming.recv() => {
                let Some(next) = next else { break; };

                match next {
                    Input::Peer(signal) => {
                        if leg.handle(signal).await {
                            retry_at = None;
                            failures = 0;
                        }
                    }
                    // 手工码模式：candidate 不走信令（它们随 SDP 一起进码），
                    // 由 `Gathered` 那一条带出去
                    Input::Signal(_signal) if leg.manual => {}
                    Input::Signal(signal) => leg.emit_signal(signal),
                    Input::Begin { offerer, features } => leg.begin(offerer, features).await,
                    Input::GatheringComplete => {
                        leg.gathering_complete = true;
                        leg.emit_code(false).await;
                    }
                    Input::Inbound(lane, bytes) => {
                        let _ = leg.events.send(P2pEvent::Inbound(lane, bytes));
                    }
                    Input::Outbound(lane, frame) => leg.send(lane, frame).await,
                    Input::Adopt(channel) => {
                        // R32：通道由**远端**创建，所以先看它的 label；只有我们声明过、
                        // 且对端也声明过的能力才认领。未知 label 一律不接管——不认领的
                        // 通道我们不会去 poll 它，也就不会去争 `Input::Inbound` 的归属。
                        let label = channel.label().await.unwrap_or_default();

                        if let Some(lane) = leg.acceptable_lane(&label) {
                            leg.adopt(lane, Arc::clone(&channel));
                            configure_flow_control(&channel).await;
                            pump(
                                channel,
                                driver.clone(),
                                lane,
                                Arc::clone(&leg.writable),
                            );
                        }
                    }
                    Input::Ready(lane) => {
                        retry_at = None;
                        failures = 0;

                        info!("P2P 通道就绪：{lane:?}");

                        let _ = leg.events.send(P2pEvent::ChannelOpen(lane));
                    }
                    Input::Failed => {
                        // 两条通道在同一条 SCTP 关联上，所以「这一轮废了」就是两条都不可用。
                        // 调用方按 lane 各自复位自己的标志。
                        let _ = leg.events.send(P2pEvent::ChannelClosed(Lane::Replaceable));
                        let _ = leg.events.send(P2pEvent::ChannelClosed(Lane::Reliable));
                        let _ = leg.events.send(P2pEvent::Unreachable);
                        // 这一轮没了，发送缓冲的判断也跟着作废，别把「不可写」留给下一轮
                        leg.writable.store(true, Ordering::Relaxed);
                        leg.reset().await;

                        // 只有发起方能重开一轮：被动一方等对方的 offer，自己重试没意义。
                        // 同一轮会连着收到好几次 `Failed`（两条通道各一次 + 连接状态），
                        // 已经排了下一轮就别再改时间，也别重复计数。
                        //
                        // 手工码模式**不重试**：重开一轮就是新的 offer，那串码对方手里没有，
                        // 而对方已经交回来的回码也随之作废——静默换码只会让人更糊涂。
                        // 这一轮废了就停在 `failed`，让用户重新出一次码。
                        if !leg.manual && leg.offerer && leg.peer_ready {
                            if retry_at.is_none() {
                                let delay = retry_delay(failures);

                                info!("P2P 这一轮没打通，{} 秒后重试", delay.as_secs());

                                retry_at = Some(tokio::time::Instant::now() + delay);
                                failures = failures.saturating_add(1);
                            }
                        } else {
                            retry_at = None;
                        }
                    }
                    Input::Stop => break,
                }
            }
            _ = retry => {
                retry_at = None;
                leg.start().await;
            }
            _ = code_wait => {
                leg.code_deadline = None;
                leg.emit_code(true).await;
            }
        }
    }

    leg.reset().await;
}

/// 一轮协商。所有失败都只走到「丢掉这一轮」，绝不 panic。
struct Leg {
    /// 自己的 deviceId，兼作 glare 的裁决
    device_id: String,
    ice_servers: Vec<IceServer>,
    /// 把「这一轮废了」送回驱动循环
    driver: mpsc::UnboundedSender<Input>,
    /// 往 [`P2pEvent`] 里写东西（这里只写 `Signal`，其余由驱动循环写）
    events: mpsc::UnboundedSender<P2pEvent>,
    /// 收到过对端的 hello
    peer_ready: bool,
    /// 这一轮由我们发起 offer（deviceId 字典序较小的一方）
    offerer: bool,
    peer: Option<Arc<dyn PeerConnection>>,
    /// `pet-state` 通道（可覆盖流）
    pet_state: Option<Arc<dyn DataChannel>>,
    /// `reliable` 通道（Phase 10）：只有对端声明了能力才会存在
    reliable: Option<Arc<dyn DataChannel>>,
    /// 对端在 `hello` 里声明过的能力（R32）
    peer_features: Vec<String>,
    /// 手工码模式：不走中继信令，offer / answer 收集完之后打成一串码由用户转送
    manual: bool,
    /// 手工码模式：本端候选已经收集完（`Complete`）
    gathering_complete: bool,
    /// 手工码模式：这一轮欠一个码，且还没交出去
    code_pending: Option<ManualCodeKind>,
    /// 手工码模式：码已经交过一次（时限兜底不能重复发）
    code_sent: bool,
    /// 手工码模式：等候选收集完成的兜底时限（`Complete` 可能永远不来，见
    /// [`CODE_GATHER_TIMEOUT`]）
    code_deadline: Option<tokio::time::Instant>,
    /// 与 [`P2pLink`] 共享的「可靠通道还能不能收帧」标志（高 / 低水位事件在维护它）
    writable: Arc<AtomicBool>,
    /// 远端描述已经设好，candidate 可以立刻加了
    remote_ready: bool,
    /// 远端描述还没设好时收到的 candidate 先攒着
    buffered_candidates: Vec<RTCIceCandidateInit>,
}

impl Leg {
    /// 手工码模式的开场：角色与能力位由码决定，不等 `hello`。
    async fn begin(&mut self, offerer: bool, features: Vec<String>) {
        self.manual = true;
        self.peer_ready = true;
        self.peer_features = features;
        self.offerer = offerer;

        let _ = self.events.send(P2pEvent::Negotiating);

        self.reset().await;
        self.start().await;
    }

    /// 手工码模式：把这一轮的本地描述（含已收集候选）交出去打码。
    ///
    /// `force = false` 要等 `Complete`，等不到就排一条 [`CODE_GATHER_TIMEOUT`] 的兜底；
    /// `force = true` 就是那条兜底到点了（带上那时候已有的候选，有多少算多少）。
    async fn emit_code(&mut self, force: bool) {
        if !self.manual || self.code_sent || self.code_pending.is_none() {
            return;
        }

        if !force && !self.gathering_complete {
            // 「收集完成」**不一定来**：STUN 完全不响应时底层 gatherer 不会把没响应的
            // client 摘表，`Complete` 就永远不来（见 `docs/pair-plan-manual-code.md`）。
            // 这里排一条按时限主动取快照的兜底，`emit_code(true)` 会带上当时已有的候选。
            self.code_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + CODE_GATHER_TIMEOUT);

            return;
        }

        let Some(kind) = self.code_pending else {
            return;
        };

        let Some(peer) = self.peer.clone() else {
            return;
        };

        let Some(description) = peer.local_description().await else {
            return;
        };

        let Some(description) = encode_description(&description) else {
            return;
        };

        self.code_sent = true;
        self.code_deadline = None;

        let (candidates, non_host) = candidate_stats(&description);

        let _ = self.events.send(P2pEvent::Gathered {
            kind,
            candidates,
            non_host,
            description,
        });
    }

    fn announce(&self) {
        self.emit_signal(PairSignalPayload::Hello {
            version: SIGNAL_VERSION,
            device_id: self.device_id.clone(),
            features: super::protocol::local_features(),
        });
    }

    /// 对端声明过第二条通道的能力（R32）。**只门控对端**：我们自己的声明在我们发出的
    /// hello 里，与这里无关。
    fn peer_supports_reliable(&self) -> bool {
        self.peer_features
            .iter()
            .any(|feature| feature == FEATURE_RELIABLE_CHANNEL)
    }

    /// 这条 label 的通道我们收不收：`pet-state` 永远收；`reliable` 只在**对端也声明过**
    /// 时才收（否则对面是旧客户端，多给它一条通道会让它抢走 `pet-state` 的出站方向）。
    fn acceptable_lane(&self, label: &str) -> Option<Lane> {
        match label {
            PET_STATE_LABEL => Some(Lane::Replaceable),
            RELIABLE_LABEL if self.peer_supports_reliable() => Some(Lane::Reliable),
            _ => None,
        }
    }

    fn channel(&self, lane: Lane) -> Option<Arc<dyn DataChannel>> {
        match lane {
            Lane::Replaceable => self.pet_state.clone(),
            Lane::Reliable => self.reliable.clone(),
        }
    }

    fn adopt(&mut self, lane: Lane, channel: Arc<dyn DataChannel>) {
        match lane {
            Lane::Replaceable => self.pet_state = Some(channel),
            Lane::Reliable => self.reliable = Some(channel),
        }
    }

    fn emit_signal(&self, signal: PairSignalPayload) {
        let _ = self.events.send(P2pEvent::Signal(signal));
    }

    /// 处理一条对端信令。返回 `true` 表示这一轮（重新）开始了，驱动循环据此清掉待重试。
    async fn handle(&mut self, signal: PairSignalPayload) -> bool {
        match signal {
            PairSignalPayload::Hello {
                version,
                device_id,
                features,
            } => {
                if version != SIGNAL_VERSION {
                    // 版本对不上就不协商：一直在中继上，好过半懂不懂地打洞
                    return false;
                }

                self.peer_ready = true;
                self.peer_features = features;
                // 字典序小的一方发起 offer：两边都算得出同一个结论，不会同时 offer
                self.offerer = self.device_id.as_str() < device_id.as_str();

                let _ = self.events.send(P2pEvent::Negotiating);

                // 对端的 hello 每次都会重开一轮——它只在开会话与重连时发一次
                self.reset().await;
                self.start().await;

                true
            }
            PairSignalPayload::Offer { description } => {
                if self.peer.is_none() {
                    // 对面直接发 offer（我们的 hello 可能还没到）：照样接。
                    // 能力门控只挡「我们主动发起」，不该挡「对方已经发起了」。
                    self.peer_ready = true;
                    self.start().await;
                }

                let Some(peer) = self.peer.clone() else {
                    return false;
                };

                let Ok(offer) = serde_json::from_str::<RTCSessionDescription>(&description) else {
                    return false;
                };

                if peer.set_remote_description(offer).await.is_err() {
                    return false;
                }

                self.remote_ready = true;
                self.flush_candidates().await;

                let Ok(answer) = peer.create_answer(None).await else {
                    return false;
                };

                if peer.set_local_description(answer.clone()).await.is_err() {
                    return false;
                }

                if self.manual {
                    // 手工码模式：回码同样等候选收集完（对方粘贴它之后 ICE 才开始）
                    self.code_pending = Some(ManualCodeKind::Answer);
                    self.emit_code(false).await;
                } else if let Some(description) = encode_description(&answer) {
                    self.emit_signal(PairSignalPayload::Answer { description });
                }

                false
            }
            PairSignalPayload::Answer { description } => {
                let Some(peer) = self.peer.clone() else {
                    return false;
                };

                let Ok(answer) = serde_json::from_str::<RTCSessionDescription>(&description) else {
                    return false;
                };

                if peer.set_remote_description(answer).await.is_err() {
                    return false;
                }

                self.remote_ready = true;
                self.flush_candidates().await;

                false
            }
            PairSignalPayload::Candidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                if !candidate.trim().is_empty() {
                    info!("P2P 远端候选：{}", describe_candidate(&candidate));
                }

                let init = RTCIceCandidateInit {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                    ..Default::default()
                };

                if !self.remote_ready {
                    // `add_ice_candidate` 在远端描述还没设好时会失败，而候选是 PC 回调
                    // 任务发出来的，顺序不保证排在 offer / answer 之后
                    self.buffered_candidates.push(init);

                    return false;
                }

                if let Some(peer) = self.peer.clone() {
                    let _ = peer.add_ice_candidate(init).await;
                }

                false
            }
        }
    }

    /// 建一条 `PeerConnection`。发起方还会建 `pet-state` 通道并发 offer。
    async fn start(&mut self) {
        // 一轮协商的现场：有了这一行，事后才能看出「什么时候开始连、用的哪些服务器」
        info!(
            "P2P 开始协商（{}）：ICE 服务器 {} 个 [{}]",
            if self.offerer {
                "本端发起 offer"
            } else {
                "本端等 offer"
            },
            self.ice_servers.len(),
            describe_ice_servers(&self.ice_servers),
        );

        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(self.ice_servers.iter().map(to_rtc_ice_server).collect())
            .build();
        let handler = Arc::new(Handler {
            driver: self.driver.clone(),
        });

        // 通配地址而不是回环：绑回环收不到真实 host 候选（R24-1）。显式绑而不是留空，
        // 是因为 `PeerConnectionBuilder<A>` 的 `A` 留空会推不出类型。
        let wildcard = std::net::SocketAddr::from(([0, 0, 0, 0], 0));

        // R32：不配上限时 `send` 永不阻塞，慢链路下就是内存无界增长。配了之后 `send` 会
        // 等到缓冲低于上限，配合 `writable` 标志在源头挡住注入。
        let builder = PeerConnectionBuilder::<std::net::SocketAddr>::new()
            .with_configuration(configuration)
            .with_handler(handler)
            .with_udp_addrs(vec![wildcard])
            .with_data_channel_send_buffer_limit(SEND_BUFFER_LIMIT);

        // 手工码模式放宽 ICE 的失败时限（见上面那两个常量）：粘贴方的 30 秒窗口太紧，
        // 而这一段码要靠人转送。中继模式不动——那条腿越快判失败越好。
        let builder = if self.manual {
            builder.with_setting_engine(
                SettingEngineBuilder::new()
                    .with_ice_timeouts(
                        Some(MANUAL_DISCONNECTED_TIMEOUT),
                        Some(MANUAL_FAILED_TIMEOUT),
                        None,
                    )
                    .build(),
            )
        } else {
            builder
        };

        let peer: Arc<dyn PeerConnection> = match builder.build().await {
                Ok(peer) => Arc::new(peer),
                Err(error) => {
                    warn!("P2P 建连失败: {error}");
                    self.fail();

                    return;
                }
            };

        self.peer = Some(Arc::clone(&peer));
        self.remote_ready = false;
        self.buffered_candidates.clear();

        if !self.offerer {
            // 被动一方只等 offer：通道由对方创建，在 SDP 里协商过来
            return;
        }

        // 通道必须在 offer 之前建，否则 SCTP 的 m 行不会进 offer
        let init = RTCDataChannelInit {
            ordered: false,
            max_retransmits: Some(0),
            ..Default::default()
        };

        match peer.create_data_channel(PET_STATE_LABEL, Some(init)).await {
            Ok(channel) => {
                self.adopt(Lane::Replaceable, Arc::clone(&channel));
                configure_flow_control(&channel).await;
                pump(
                    channel,
                    self.driver.clone(),
                    Lane::Replaceable,
                    Arc::clone(&self.writable),
                );
            }
            Err(error) => {
                warn!("P2P 建通道失败: {error}");
                self.fail();

                return;
            }
        }

        // 第二条通道（Phase 10）。**只在对面声明过能力时才建**：对端是旧客户端时多建一条
        // 会让它的 `on_data_channel` 无条件认领、抢走 `pet-state` 的出站方向（R32）。
        //
        // 手工码模式是例外：出码方建 offer 的时候**还不知道**对方的能力位（那要等对方交回
        // 码 2，而 offer 已经在码 1 里发出去了），只能无条件建。这不违反 R32 的本意——配对
        // 码自带版本号，能出码 / 能解开的必然是同一版客户端，不存在「旧客户端那一侧」；
        // 真不认领时这条通道只是不开，可覆盖流照旧。
        if self.peer_supports_reliable() || self.manual {
            // 有序 + 不限重传次数 = 可靠。crate 的 `Default` 已经是这个形状，这里显式写
            // 出来是为了抗上游改默认值（与依赖显式写 `features` 同一个理由）。
            let init = RTCDataChannelInit {
                ordered: true,
                ..Default::default()
            };

            match peer.create_data_channel(RELIABLE_LABEL, Some(init)).await {
                Ok(channel) => {
                    self.adopt(Lane::Reliable, Arc::clone(&channel));
                    configure_flow_control(&channel).await;
                    pump(
                        channel,
                        self.driver.clone(),
                        Lane::Reliable,
                        Arc::clone(&self.writable),
                    );
                }
                Err(error) => {
                    // 只丢这一条通道：P2P 本身还能用（可覆盖流照旧），别把整轮打掉
                    warn!("P2P 建可靠通道失败: {error}");
                }
            }
        }

        match peer.create_offer(None).await {
            Ok(offer) => {
                if let Err(error) = peer.set_local_description(offer.clone()).await {
                    warn!("P2P 设置本地描述失败: {error}");
                    self.fail();

                    return;
                }

                // 手工码模式不进信令：这一轮的码等候选收集完（或时限到了）再交出去
                if self.manual {
                    self.code_pending = Some(ManualCodeKind::Offer);
                    self.emit_code(false).await;

                    return;
                }

                // 不等 ICE 收完：candidate 单独 trickle 过去（信令只有个位数帧，见 R26-3）
                match encode_description(&offer) {
                    Some(description) => self.emit_signal(PairSignalPayload::Offer { description }),
                    None => self.fail(),
                }
            }
            Err(error) => {
                warn!("P2P 生成 offer 失败: {error}");
                self.fail();
            }
        }
    }

    /// 丢掉这一轮。`close()` 失败也只能忽略——它已经把本地状态收了，剩下的交给 GC。
    async fn reset(&mut self) {
        self.remote_ready = false;
        self.buffered_candidates.clear();
        self.gathering_complete = false;
        self.code_pending = None;
        self.code_sent = false;
        self.code_deadline = None;
        self.pet_state = None;
        self.reliable = None;
        // 背压标志是**这一轮**的：新通道的 SCTP 发送缓冲从 0 开始只增不减，而 High / Low
        // 都是「跨过阈值」的边沿事件——旧通道留下的 `false` 在新通道上不会再被翻回来
        // （标志只在 `Input::Failed` 里复位，那只是本轮的副作用，不是轮次边界）。
        // 不复位的话：可靠帧整体退回中继（无害），但钉在 DC 上的那一单永远发不出分片，
        // 而这条腿 open + verified、探针也正常，`direct_lost()` 两个触发点都到不了。
        self.writable.store(true, Ordering::Relaxed);

        if let Some(peer) = self.peer.take() {
            let _ = peer.close().await;
        }
    }

    /// 把攒着的 candidate 交给 PeerConnection（远端描述设好之后）
    async fn flush_candidates(&mut self) {
        let Some(peer) = self.peer.clone() else {
            self.buffered_candidates.clear();

            return;
        };

        for candidate in std::mem::take(&mut self.buffered_candidates) {
            let _ = peer.add_ice_candidate(candidate).await;
        }
    }

    /// 在指定的 DataChannel 上发一帧。通道没开就丢掉。
    ///
    /// 这里用的是**阻塞**的 `send`：配了发送缓冲上限之后它会等到缓冲低于上限。上游
    /// （`manager.rs` 的分片分支）用 `writable()` 在**源头**挡住了注入，所以这里最多
    /// 只会有「标志翻转前已经上路的那一块」需要等，不会无界堆积。
    async fn send(&self, lane: Lane, frame: Vec<u8>) {
        let Some(channel) = self.channel(lane) else {
            return;
        };

        if let Err(error) = channel.send(BytesMut::from(&frame[..])).await {
            warn!("P2P 发送失败: {error}");
        }
    }

    fn fail(&self) {
        let _ = self.driver.send(Input::Failed);
    }
}

/// `PeerConnection` 的回调。这里是 `panic = "abort"` 下最不能 `unwrap` 的地方。
struct Handler {
    driver: mpsc::UnboundedSender<Input>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else {
            return;
        };

        info!("P2P 本地候选：{}", describe_candidate(&init.candidate));

        let _ = self
            .driver
            .send(Input::Signal(PairSignalPayload::Candidate {
                candidate: init.candidate,
                sdp_mid: init.sdp_mid,
                sdp_mline_index: init.sdp_mline_index,
            }));
    }

    /// 手工码模式靠它决定「什么时候可以把 SDP 打成码」。**它不一定来**：STUN 完全不响应
    /// 时 gathering 会一直停在 `InProgress`（见 `docs/pair-plan-manual-code.md`），所以
    /// 驱动循环里还排了一条 [`CODE_GATHER_TIMEOUT`] 的兜底：到点直接走 `emit_code(true)`,
    /// 带上那时候已经收集到的候选。
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if matches!(state, RTCIceGatheringState::Complete) {
            let _ = self.driver.send(Input::GatheringComplete);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        // R21：立即失败条件是「DC 或 ICE 进入 failed/closed」。`Disconnected` 不算——
        // 它是暂时的，会自己恢复，当成失败会让两边反复重建。
        info!("P2P 连接状态：{state:?}");

        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            let _ = self.driver.send(Input::Failed);
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        // `Connected` = 至少一对候选通了；`Completed` = 最终那一对已经选定
        info!("P2P ICE 状态：{state:?}");
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        // 本端创建的通道不会触发这个回调；只有远端创建的会，所以它一定是对方那条
        // `pet-state`。交给驱动循环接管（它同时负责起泵）。
        let _ = self.driver.send(Input::Adopt(channel));
    }
}

/// 给一条 DataChannel 配好背压的水位线（R32）。
///
/// 高水位 = 发送缓冲上限本身（一越线就让 `writable` 翻假）；低水位 = 一个分片。
/// 只有可靠那条通道用到这两个事件，可覆盖流那套是「最新值赢」，压不住就丢一帧。
async fn configure_flow_control(channel: &Arc<dyn DataChannel>) {
    // 失败只意味着通道已经在关：那就交给 `OnClose` / `Input::Failed` 那条路
    let _ = channel
        .set_buffered_amount_high_threshold(SEND_BUFFER_HIGH)
        .await;
    let _ = channel
        .set_buffered_amount_low_threshold(SEND_BUFFER_LOW)
        .await;
}

/// 把一条 DataChannel 的事件泵出去。
///
/// 本端创建的与从 `on_data_channel` 拿到的都走这里，两条路径的行为必须一致。
fn pump(
    channel: Arc<dyn DataChannel>,
    driver: mpsc::UnboundedSender<Input>,
    lane: Lane,
    writable: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    let _ = driver.send(Input::Ready(lane));
                }
                DataChannelEvent::OnMessage(message) => {
                    // 所有应用数据都走 Binary（与中继那条路一致）
                    if !message.is_string {
                        let _ = driver.send(Input::Inbound(lane, message.data.to_vec()));
                    }
                }
                // 只有可靠那条通道吃背压：分片被拒 = 跳号 = 整单报废，而可覆盖流是
                // 「最新值赢」，压不住就丢一帧
                DataChannelEvent::OnBufferedAmountLow if lane == Lane::Reliable => {
                    writable.store(true, Ordering::Relaxed);
                }
                DataChannelEvent::OnBufferedAmountHigh if lane == Lane::Reliable => {
                    writable.store(false, Ordering::Relaxed);
                }
                DataChannelEvent::OnClose => break,
                // OnClosing / OnError / OnBufferedAmount*：要么紧随其后就是 OnClose，
                // 要么与本层的用法无关
                _ => {}
            }
        }

        let _ = driver.send(Input::Failed);
    });
}

/// 把中继广告的 ICE 服务器转成 webrtc 的类型。空条目丢掉——空 URL 会让打洞直接失败。
fn to_rtc_ice_server(server: &IceServer) -> RTCIceServer {
    RTCIceServer {
        urls: server
            .urls
            .iter()
            .filter(|url| !url.trim().is_empty())
            .cloned()
            .collect(),
        username: server.username.clone(),
        credential: server.credential.clone(),
    }
}

/// 把 `RTCSessionDescription` 序列化进信令。
///
/// 描述里既有 SDP 文本也有类型（offer / answer），两者都要带过去，所以整体走 serde，
/// 而不是只发 SDP 字符串。
fn encode_description(description: &RTCSessionDescription) -> Option<String> {
    serde_json::to_string(description).ok()
}

#[cfg(test)]
mod tests {
    use super::super::crypto::PairCipher;
    use super::super::protocol::{FRAME_HEADER_SIZE, FrameHeader, FrameKind, NONCE_SIZE};
    use super::super::transfer::P2P_CHUNK_SIZE;
    use super::*;

    /// 日志里的现场必须能回答「走了哪种候选」，同时**绝不能**带出凭据。
    #[test]
    fn the_logged_site_keeps_the_facts_and_no_credentials() {
        let servers = vec![
            IceServer {
                urls: vec!["turn:cat.example.com:3478?transport=udp".to_string()],
                username: "pair-user".to_string(),
                credential: "s3cret".to_string(),
            },
            IceServer {
                urls: vec!["turn:u:p@hidden.example.com:3478".to_string()],
                username: String::new(),
                credential: String::new(),
            },
        ];

        let text = describe_ice_servers(&servers);

        assert!(text.contains("cat.example.com:3478"), "{text}");
        assert!(text.contains("hidden.example.com:3478"), "{text}");
        assert!(!text.contains("pair-user"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!text.contains("u:p@"), "{text}");
        assert!(describe_ice_servers(&[]).contains("host"));

        // `candidate:... udp 2130706431 192.168.1.9 50000 typ host ...`
        let host = describe_candidate(
            "candidate:1 1 udp 2130706431 192.168.1.9 50000 typ host generation 0",
        );

        assert_eq!(host, "host udp 192.168.1.9");
        assert_eq!(
            describe_candidate("candidate:2 1 udp 1 203.0.113.7 61000 typ relay raddr 0.0.0.0"),
            "relay udp 203.0.113.7"
        );
        assert_eq!(describe_candidate("   "), "候选收集结束");
    }

    /// 手工码那句「大概率只有同一个网络里能连」全靠它：数的是**非 host** 候选。
    ///
    /// 这里只能靠猜 SDP 行的形状（`Typ` 后面那个词），所以钉一条纯函数用例：上游哪天改了
    /// marshal 写法，或者有人把判据改回「候选总数」，这条会先红。
    #[test]
    fn candidate_stats_counts_the_non_host_candidates() {
        fn description(sdp: &str) -> String {
            serde_json::json!({ "type": "offer", "sdp": sdp }).to_string()
        }

        // 一条 host + 一条 srflx：STUN 帮我们要到了外网映射
        assert_eq!(
            candidate_stats(&description(
                "v=0\r\n\
                 a=candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host\r\n\
                 a=candidate:2 1 udp 1694498815 203.0.113.7 54321 typ srflx \
                 raddr 192.168.1.5 rport 54321\r\n\
                 a=end-of-candidates\r\n"
            )),
            (2, 1)
        );

        // 多网卡（有线 + 无线 + 虚拟网卡）没有 STUN：候选好几条，非 host 一条都没有。
        // 判据要是用候选总数，这段跨网络连不上的码就会被当成正常。
        assert_eq!(
            candidate_stats(&description(
                "a=candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host\r\n\
                 a=candidate:3 1 udp 2130706431 10.0.0.2 54322 typ host\r\n\
                 a=candidate:4 1 udp 2130706431 172.17.0.1 54323 typ host\r\n"
            )),
            (3, 0)
        );

        // 认不出来的行按 host 算：宁可少报「能连」，也不要给出一段其实连不上的码
        assert_eq!(
            candidate_stats(&description("a=candidate:9 1 udp 1 192.168.1.9 1\r\n")),
            (1, 0)
        );

        // 没有候选的 SDP、不是 JSON、以及只有 end-of-candidates：都算 0
        assert_eq!(candidate_stats(&description("v=0\r\n")), (0, 0));
        assert_eq!(candidate_stats(&description("a=end-of-candidates\r\n")), (0, 0));
        assert_eq!(candidate_stats(""), (0, 0));
        assert_eq!(candidate_stats("not json"), (0, 0));
    }

    /// 这一层用的 transfer id（值本身不重要，帧头里带上它只是为了让「错帧」看得出来）
    const TRANSFER_ID: u64 = 42;
    /// 三个用例共用的一份根密钥，不碰 `secret` / `crypto` 的跨语言固定向量。
    ///
    /// 真实路径是 `crypto::derive_transfer_key(root, transfer_id)`，这里直接拿根密钥当
    /// cipher key：用例只关心**线上长度与字节边界**，而封帧开销（帧头 + nonce + tag）
    /// 与密钥是什么完全无关。
    const ROOT_KEY: [u8; 32] = [7; 32];
    /// Poly1305 认证标签的长度（`transfer::AEAD_TAG_SIZE` 是私有的，这里只为对拍尺寸）
    const TAG_SIZE: usize = 16;

    #[test]
    fn retry_backs_off_and_caps() {
        assert_eq!(retry_delay(0), Duration::from_secs(5));
        assert_eq!(retry_delay(1), Duration::from_secs(10));
        assert_eq!(retry_delay(3), Duration::from_secs(40));
        assert_eq!(retry_delay(4), Duration::from_secs(80));
        assert_eq!(retry_delay(5), Duration::from_secs(120));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(120));
    }

    /// 自建中继的内置 STUN（`server-relay/src/stun.rs`）真的能被这套 WebRTC 栈读懂：
    /// 问它一次，就应该收集到一个 `srflx`（「服务器看到的我的地址」）候选。没有这类
    /// 候选，两个不同局域网里的人永远打不通。
    ///
    /// 要先起一个中继，再用**本机的局域网地址**指过去（回环地址收不到 host 候选对应
    /// 的响应）：`BONGO_PAIR_E2E_STUN=stun:192.168.x.x:3479`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要一个开着内置 STUN 的中继，见用例注释"]
    async fn the_relay_builtin_stun_yields_a_srflx_candidate() {
        let url = std::env::var("BONGO_PAIR_E2E_STUN").expect("先设 BONGO_PAIR_E2E_STUN");
        let server = IceServer {
            urls: vec![url],
            username: String::new(),
            credential: String::new(),
        };
        let (link, mut events) = P2pLink::spawn("a".to_string(), vec![server]);

        // 对端的 hello：「b」字典序更大，所以我们是发起方，会立刻开始收集候选
        link.handle_signal(PairSignalPayload::Hello {
            version: SIGNAL_VERSION,
            device_id: "b".to_string(),
            features: vec![FEATURE_RELIABLE_CHANNEL.to_string()],
        });

        let found = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.next().await {
                if let P2pEvent::Signal(PairSignalPayload::Candidate { candidate, .. }) = event {
                    if candidate.contains(" typ srflx") {
                        return candidate;
                    }
                }
            }

            panic!("腿提前结束了");
        })
        .await;

        assert!(found.is_ok(), "10 秒内没有收集到 srflx 候选");
    }

    /// 两条腿在同一个进程里互相对接：不经过中继，不需要第二台机器，也不需要任何外部服务。
    ///
    /// 这是这一层唯一能自动化验证的路径——跨 NAT 的打洞成功率是人工验收项（§10），
    /// 但「真的 SCTP / DataChannel 能不能把这一帧送过去」在本机就能验。
    ///
    /// 返回两条腿**与两边的接收端**：两条事件流都要留着，丢掉任何一侧的接收端都会让
    /// 那一侧的驱动循环再也送不出信令（发送失败被忽略，腿会一直停在协商中）。
    async fn connect_two_legs() -> (P2pLink, P2pLink, P2pEvents, P2pEvents) {
        let (link_a, mut events_a) = P2pLink::spawn("a".to_string(), Vec::new());
        let (link_b, mut events_b) = P2pLink::spawn("b".to_string(), Vec::new());

        let mut a_open = [false; 2];
        let mut b_open = [false; 2];

        // 把两条腿的信令互相喂过去，直到两边都报告**两条**通道都打开
        let connected = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    event = events_a.next() => match event {
                        Some(P2pEvent::Signal(signal)) => link_b.handle_signal(signal),
                        Some(P2pEvent::ChannelOpen(lane)) => a_open[lane_index(lane)] = true,
                        _ => {}
                    },
                    event = events_b.next() => match event {
                        Some(P2pEvent::Signal(signal)) => link_a.handle_signal(signal),
                        Some(P2pEvent::ChannelOpen(lane)) => b_open[lane_index(lane)] = true,
                        _ => {}
                    },
                }

                if a_open.iter().all(|open| *open) && b_open.iter().all(|open| *open) {
                    return;
                }
            }
        })
        .await;

        assert!(
            connected.is_ok(),
            "30 秒内没有打通：a_open={a_open:?} b_open={b_open:?}"
        );

        (link_a, link_b, events_a, events_b)
    }

    /// 一段可校验的伪随机负载：任意一块被重复、跳号或截断，逐字节比较都会失败
    fn source_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut bytes = Vec::with_capacity(length);

        while bytes.len() < length {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push((state >> 24) as u8);
        }

        bytes
    }

    /// 按真的分片形状封帧：`TransferChunk` + 48 KiB 明文（`manager.rs` 在 Direct 那一单
    /// 用的就是 `P2P_CHUNK_SIZE`）
    fn seal_chunks(cipher: &PairCipher, payload: &[u8]) -> Vec<Vec<u8>> {
        payload
            .chunks(P2P_CHUNK_SIZE)
            .enumerate()
            .map(|(index, chunk)| {
                let header = FrameHeader {
                    kind: FrameKind::TransferChunk,
                    flags: 0,
                    transfer_id: TRANSFER_ID,
                    seq: index as u32,
                };

                cipher.seal(&header, chunk).expect("封帧")
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_legs_negotiate_and_open_the_channel() {
        let (link_a, _link_b, _events_a, mut events_b) = connect_two_legs().await;

        // 两条通道都是双向的：A 各发一帧，B 应该在同一条 lane 上原样收到
        link_a.send(Lane::Replaceable, vec![1, 2, 3]);
        link_a.send(Lane::Reliable, vec![4, 5, 6]);

        let received = tokio::time::timeout(Duration::from_secs(10), async {
            let mut seen: [Option<Vec<u8>>; 2] = [None, None];

            loop {
                match events_b.next().await {
                    Some(P2pEvent::Inbound(lane, bytes)) => {
                        seen[lane_index(lane)] = Some(bytes);

                        if seen.iter().all(|entry| entry.is_some()) {
                            return seen;
                        }
                    }
                    Some(_) => {}
                    None => return seen,
                }
            }
        })
        .await;

        let seen = received.expect("10 秒内两条通道都该收到那一帧");

        assert_eq!(seen[0], Some(vec![1, 2, 3]), "可覆盖通道");
        assert_eq!(seen[1], Some(vec![4, 5, 6]), "可靠通道");
    }

    /// §10：48 KiB 的分片**真的**过 DataChannel。
    ///
    /// 假腿单测能证明「按 48 KiB 切块、序号连续」，证明不了这一帧过不过得了 SCTP /
    /// DataChannel（消息大小、分片重组、顺序）。这里两条腿在同一进程里，但数据全程走
    /// 真的 host candidate——**不需要第二台机器，也不需要中继**。
    ///
    /// 结尾故意多一个字节：真实附件的最后一块总是短块（`transfer::chunk_length`），
    /// 而短帧过不过得了 SCTP 是另一个问题，不能只在整块上验。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_reliable_lane_carries_real_48kib_chunks() {
        const CHUNKS: usize = 24;

        let (link_a, _link_b, _events_a, mut events_b) = connect_two_legs().await;

        let payload = source_bytes(P2P_CHUNK_SIZE * CHUNKS + 1);
        let cipher = PairCipher::new(&ROOT_KEY);
        let frames = seal_chunks(&cipher, &payload);

        // 24 块整块 + 1 个 1 字节的短块
        assert_eq!(frames.len(), CHUNKS + 1);
        assert_eq!(
            frames[0].len(),
            P2P_CHUNK_SIZE + FRAME_HEADER_SIZE + NONCE_SIZE + TAG_SIZE,
            "上线长度该是 48 KiB 明文加分片开销"
        );
        assert_eq!(
            frames.last().unwrap().len(),
            1 + FRAME_HEADER_SIZE + NONCE_SIZE + TAG_SIZE,
            "短块的上线长度只有 1 字节明文"
        );

        for frame in &frames {
            link_a.send(Lane::Reliable, frame.clone());
        }

        let received = tokio::time::timeout(Duration::from_secs(60), async {
            let mut sequence = Vec::new();
            let mut rebuilt = Vec::new();
            let mut last_length = 0;

            while sequence.len() < frames.len() {
                match events_b.next().await {
                    Some(P2pEvent::Inbound(Lane::Reliable, bytes)) => {
                        let (header, plain) = cipher.open(&bytes).expect("解密这一帧");

                        assert_eq!(header.kind, FrameKind::TransferChunk);
                        assert_eq!(header.transfer_id, TRANSFER_ID);

                        last_length = plain.len();
                        sequence.push(header.seq);
                        rebuilt.extend_from_slice(&plain);
                    }
                    Some(_) => {}
                    None => break,
                }
            }

            (sequence, rebuilt, last_length)
        })
        .await
        .expect("60 秒内 25 块都该到");

        assert_eq!(
            received.0,
            (0..(CHUNKS + 1) as u32).collect::<Vec<_>>(),
            "序号必须严格递增，一块都不能少"
        );
        assert_eq!(received.2, 1, "最后一块该是 1 字节的短块");
        assert_eq!(
            received.1.len(),
            payload.len(),
            "拼回来的长度该和源字节一样"
        );
        assert_eq!(received.1, payload, "拼回来必须逐字节一致");
    }

    /// §10 / R32：DC 的发送缓冲真的会压到上限、也真的能排空翻回来（`writable`）。
    ///
    /// 灌进去的分片远多于 192 KiB 的发送缓冲上限，所以「压满 → `writable` 翻假」与
    /// 「排空 → 低水位事件翻回真」这两件事都能在本机跑出来：真的 SCTP 发送缓冲就在这个
    /// 进程里。压满之后逐字节一致，说明等待期间一块都没被丢掉或写坏。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_send_buffer_really_fills_and_drains() {
        /// 200 块 48 KiB ≈ 9.4 MiB，远多于 192 KiB 的上限
        const CHUNKS: usize = 200;

        let (link_a, _link_b, _events_a, mut events_b) = connect_two_legs().await;

        let payload = source_bytes(P2P_CHUNK_SIZE * CHUNKS);
        let cipher = PairCipher::new(&ROOT_KEY);
        let frames = seal_chunks(&cipher, &payload);

        assert!(link_a.writable(), "刚开好的通道该是可写的");

        // 一口气灌进去：驱动循环一边发一边等缓冲，越过上限就会把标志翻假
        for frame in &frames {
            link_a.send(Lane::Reliable, frame.clone());
        }

        let blocked = tokio::time::timeout(Duration::from_secs(30), async {
            while link_a.writable() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;

        assert!(
            blocked.is_ok(),
            "灌了 {} KiB 都没把发送缓冲压到上限（{} KiB）：背压等于没生效",
            payload.len() / 1024,
            SEND_BUFFER_LIMIT / 1024
        );

        let received = tokio::time::timeout(Duration::from_secs(120), async {
            let mut rebuilt = Vec::new();

            while rebuilt.len() < payload.len() {
                match events_b.next().await {
                    Some(P2pEvent::Inbound(Lane::Reliable, bytes)) => {
                        let (_, plain) = cipher.open(&bytes).expect("解密这一帧");

                        rebuilt.extend_from_slice(&plain);
                    }
                    Some(_) => {}
                    None => break,
                }
            }

            rebuilt
        })
        .await
        .expect("120 秒内 200 块都该到");

        assert_eq!(received, payload, "压满再排空之后拼回来必须逐字节一致");

        let drained = tokio::time::timeout(Duration::from_secs(30), async {
            while !link_a.writable() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;

        assert!(
            drained.is_ok(),
            "排空之后背压没有翻回来：低水位事件（OnBufferedAmountLow）没到"
        );
    }

    fn lane_index(lane: Lane) -> usize {
        match lane {
            Lane::Replaceable => 0,
            Lane::Reliable => 1,
        }
    }

    /// §10 的「中途拔掉 P2P」在**线上格式**那一半：真的把腿拔掉时，对面只会少收整块，
    /// 绝不会收到半块、错块或乱序的块。
    ///
    /// 会话层那一半（`direct_lost()` → 钉在 DC 上的那一单按 §43 失败、中继上补一条
    /// `transfer.cancel`、中继那一单不受影响）由 `manager.rs` 的
    /// `losing_the_direct_leg_fails_its_transfer_and_cancels_it_over_the_relay` 覆盖：
    /// 会话层没有真的「拔腿」入口，所以那个动作只能直接调用。这里管的是接收侧看到的
    /// 字节边界。
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_leg_mid_burst_never_delivers_a_torn_chunk() {
        const CHUNKS: usize = 8;
        /// 先收到几块再拔线：太早拔可能一块都还没到，就测不出「线上的块是完整的」
        const EARLY: usize = 2;

        let (link_a, _link_b, _events_a, mut events_b) = connect_two_legs().await;

        let payload = source_bytes(P2P_CHUNK_SIZE * CHUNKS);
        let cipher = PairCipher::new(&ROOT_KEY);
        let frames = seal_chunks(&cipher, &payload);

        for frame in &frames {
            link_a.send(Lane::Reliable, frame.clone());
        }

        let early = tokio::time::timeout(Duration::from_secs(30), async {
            let mut early = Vec::new();

            while early.len() < EARLY {
                match events_b.next().await {
                    Some(P2pEvent::Inbound(Lane::Reliable, bytes)) => early.push(bytes),
                    Some(_) => {}
                    None => break,
                }
            }

            early
        })
        .await
        .expect("30 秒内前两块该到");

        assert_eq!(early.len(), EARLY, "拔线之前该已经收到前两块");

        // 拔线：`Drop` 发的 `Input::Stop` 与上面那 8 帧走的是同一条 FIFO，所以驱动循环会先把
        // 8 帧交给 SCTP 的发送缓冲、再关掉 PeerConnection（已经上路的帧仍可能到对端；
        // 到不了的只是没发出去的那部分）。
        drop(link_a);

        // 接收侧多久之后才**发现**对端没了（ICE 的 consent freshness 是几十秒量级）不由这一层
        // 决定，所以这里只做一个有界窗口的观察：**窗口里到过**的每一块都必须是整块且有序的。
        let late = tokio::time::timeout(Duration::from_secs(2), async {
            let mut late = Vec::new();

            while let Some(event) = events_b.next().await {
                if let P2pEvent::Inbound(Lane::Reliable, bytes) = event {
                    late.push(bytes);
                }
            }

            late
        })
        .await
        .unwrap_or_default();

        let received: Vec<Vec<u8>> = early.into_iter().chain(late).collect();

        assert!(
            received.len() <= CHUNKS,
            "收到 {} 块，超过发出去的 {CHUNKS} 块",
            received.len()
        );

        let mut rebuilt = Vec::new();

        for (index, bytes) in received.iter().enumerate() {
            // 解不开就是线上格式被截断或被拼接了——接收侧会把它当成另一单的分片
            let (header, plain) = cipher.open(bytes).expect("掉线不能送出半块：这一帧解不开");

            assert_eq!(header.seq, index as u32, "拔线的间隙也不能乱序");
            assert_eq!(plain.len(), P2P_CHUNK_SIZE, "每一块都该是整块");

            rebuilt.extend_from_slice(&plain);
        }

        // 收到的那几块必须是源字节的**前缀**：少收可以（对端拔线了），错位不行
        assert_eq!(
            rebuilt,
            payload[..rebuilt.len()].to_vec(),
            "收到的那几块该是源字节的前缀"
        );
    }
}

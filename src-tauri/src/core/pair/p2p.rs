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
use tauri_plugin_log::log::warn;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription,
};

use super::link::Lane;
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

/// 一轮协商失败后，发起方隔多久再试一次
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// 驱动循环的输入。PC 回调、会话层交给它的对端信令、会话层要发出去的字节，都走这一条
/// 通道；驱动循环是唯一会向 [`P2pEvent`] 里写东西的地方（`Leg` 只发信令）。
enum Input {
    /// 中继上收到的**对端**信令，交给这一轮协商
    Peer(PairSignalPayload),
    /// 本层要送出去的信号（PC 回调产生的 candidate）
    Signal(PairSignalPayload),
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
    /// `ice_servers` 来自 `server.welcome` 的广告（R21）；空表示只有 host candidate，
    /// 这是隐私缺省（不填任何公共 STUN）。
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
        writable,
        remote_ready: false,
        buffered_candidates: Vec::new(),
    };
    let mut retry_at: Option<tokio::time::Instant> = None;

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

        tokio::select! {
            next = incoming.recv() => {
                let Some(next) = next else { break; };

                match next {
                    Input::Peer(signal) => {
                        if leg.handle(signal).await {
                            retry_at = None;
                        }
                    }
                    Input::Signal(signal) => leg.emit_signal(signal),
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
                        let _ = leg.events.send(P2pEvent::ChannelOpen(lane));
                    }
                    Input::Failed => {
                        // 两条通道在同一条 SCTP 关联上，所以「这一轮废了」就是两条都不可用。
                        // 调用方按 lane 各自复位自己的标志。
                        let _ = leg.events.send(P2pEvent::ChannelClosed(Lane::Replaceable));
                        let _ = leg.events.send(P2pEvent::ChannelClosed(Lane::Reliable));
                        // 这一轮没了，发送缓冲的判断也跟着作废，别把「不可写」留给下一轮
                        leg.writable.store(true, Ordering::Relaxed);
                        leg.reset().await;

                        // 只有发起方能重开一轮：被动一方等对方的 offer，自己重试没意义
                        retry_at = if leg.offerer && leg.peer_ready {
                            Some(tokio::time::Instant::now() + RETRY_DELAY)
                        } else {
                            None
                        };
                    }
                    Input::Stop => break,
                }
            }
            _ = retry => {
                retry_at = None;
                leg.start().await;
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
    /// 与 [`P2pLink`] 共享的「可靠通道还能不能收帧」标志（高 / 低水位事件在维护它）
    writable: Arc<AtomicBool>,
    /// 远端描述已经设好，candidate 可以立刻加了
    remote_ready: bool,
    /// 远端描述还没设好时收到的 candidate 先攒着
    buffered_candidates: Vec<RTCIceCandidateInit>,
}

impl Leg {
    fn announce(&self) {
        self.emit_signal(PairSignalPayload::Hello {
            version: SIGNAL_VERSION,
            device_id: self.device_id.clone(),
            features: vec![FEATURE_RELIABLE_CHANNEL.to_string()],
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

                if let Some(description) = encode_description(&answer) {
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
        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(self.ice_servers.iter().map(to_rtc_ice_server).collect())
            .build();
        let handler = Arc::new(Handler {
            driver: self.driver.clone(),
        });

        // 通配地址而不是回环：绑回环收不到真实 host 候选（R24-1）。显式绑而不是留空，
        // 是因为 `PeerConnectionBuilder<A>` 的 `A` 留空会推不出类型。
        let wildcard = std::net::SocketAddr::from(([0, 0, 0, 0], 0));

        let peer: Arc<dyn PeerConnection> =
            match PeerConnectionBuilder::<std::net::SocketAddr>::new()
                .with_configuration(configuration)
                .with_handler(handler)
                .with_udp_addrs(vec![wildcard])
                // R32：不配上限时 `send` 永不阻塞，慢链路下就是内存无界增长。
                // 配了之后 `send` 会等到缓冲低于上限，配合 `writable` 标志在源头挡住注入。
                .with_data_channel_send_buffer_limit(SEND_BUFFER_LIMIT)
                .build()
                .await
            {
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
        if self.peer_supports_reliable() {
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

        let _ = self
            .driver
            .send(Input::Signal(PairSignalPayload::Candidate {
                candidate: init.candidate,
                sdp_mid: init.sdp_mid,
                sdp_mline_index: init.sdp_mline_index,
            }));
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        // R21：立即失败条件是「DC 或 ICE 进入 failed/closed」。`Disconnected` 不算——
        // 它是暂时的，会自己恢复，当成失败会让两边反复重建。
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            let _ = self.driver.send(Input::Failed);
        }
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
    use super::*;

    /// 两条腿在同一个进程里互相对接：不经过中继，也不需要任何外部服务。
    ///
    /// 这是这一层唯一能自动化验证的路径——真机双端打洞是人工验收项（§10）。
    #[tokio::test(flavor = "multi_thread")]
    async fn two_legs_negotiate_and_open_the_channel() {
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

    fn lane_index(lane: Lane) -> usize {
        match lane {
            Lane::Replaceable => 0,
            Lane::Reliable => 1,
        }
    }
}

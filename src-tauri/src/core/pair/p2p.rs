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

use super::protocol::{IceServer, PairSignalPayload, SIGNAL_VERSION};

/// Phase 8 只启用这一条通道（§5.2）：`ordered = false, maxRetransmits = 0`，只跑可覆盖流。
/// 聊天、附件、控制留在中继——附件分片绝不能走这里（接收侧要求分片序号严格递增，
/// 而这条通道会丢帧，见 R21）。
const PET_STATE_LABEL: &str = "pet-state";

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
    Inbound(Vec<u8>),
    /// 在当前 DataChannel 上发一帧
    Outbound(Vec<u8>),
    /// 远端开了一条通道（`on_data_channel`），交给驱动循环接管
    Adopt(Arc<dyn DataChannel>),
    /// DataChannel 开了
    Ready,
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
    /// DataChannel 收到的原始字节，交给 `manager.rs` 的 `handle_binary`
    Inbound(Vec<u8>),
    /// DataChannel 可以用了：探针开始（Phase 8c 起也是「可以把可覆盖流切过来」的信号）
    ChannelOpen,
    /// DataChannel 不能用了。**重复收到是无害的**（没开过也会收到，比如 ICE 直接失败），
    /// 调用方按「现在不可用」处理即可。
    ChannelClosed,
}

/// 一条 P2P 腿。会话层持有它；丢掉它即收摊（见 `Drop`）。
///
/// 事件流是**分开**的对象（[`P2pEvents`]）：`live` 的 `select!` 要一边用 `&mut` 轮询
/// 事件、一边用 `&` 发信令，同一个值没法同时被可变借用两次。
pub struct P2pLink {
    input: mpsc::UnboundedSender<Input>,
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

        // 驱动循环自己也要往 `Input` 里发（接管远端通道时要起泵），所以留一份发送端
        tokio::spawn(drive(
            device_id,
            ice_servers,
            input.clone(),
            incoming,
            events,
        ));

        (Self { input }, P2pEvents { events: event_rx })
    }

    /// 把中继上收到的对端信令交给这条腿。腿已经收摊就丢掉——P2P 是尽力而为的加速层，
    /// 丢了不影响中继上的功能。
    pub fn handle_signal(&self, signal: PairSignalPayload) {
        let _ = self.input.send(Input::Peer(signal));
    }

    /// 在当前 DataChannel 上发一帧。通道没开就丢掉（同上，尽力而为）。
    pub fn send(&self, frame: Vec<u8>) {
        let _ = self.input.send(Input::Outbound(frame));
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
) {
    let mut leg = Leg {
        device_id,
        ice_servers,
        driver,
        events,
        peer_ready: false,
        offerer: false,
        peer: None,
        channel: None,
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
                    Input::Inbound(bytes) => {
                        let _ = leg.events.send(P2pEvent::Inbound(bytes));
                    }
                    Input::Outbound(frame) => leg.send(frame).await,
                    Input::Adopt(channel) => {
                        leg.channel = Some(Arc::clone(&channel));
                        pump(channel, driver.clone());
                    }
                    Input::Ready => {
                        retry_at = None;
                        let _ = leg.events.send(P2pEvent::ChannelOpen);
                    }
                    Input::Failed => {
                        let _ = leg.events.send(P2pEvent::ChannelClosed);
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
    channel: Option<Arc<dyn DataChannel>>,
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
        });
    }

    fn emit_signal(&self, signal: PairSignalPayload) {
        let _ = self.events.send(P2pEvent::Signal(signal));
    }

    /// 处理一条对端信令。返回 `true` 表示这一轮（重新）开始了，驱动循环据此清掉待重试。
    async fn handle(&mut self, signal: PairSignalPayload) -> bool {
        match signal {
            PairSignalPayload::Hello { version, device_id } => {
                if version != SIGNAL_VERSION {
                    // 版本对不上就不协商：一直在中继上，好过半懂不懂地打洞
                    return false;
                }

                self.peer_ready = true;
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
                self.channel = Some(Arc::clone(&channel));
                pump(channel, self.driver.clone());
            }
            Err(error) => {
                warn!("P2P 建通道失败: {error}");
                self.fail();

                return;
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
        self.channel = None;

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

    /// 在当前 DataChannel 上发一帧。通道没开就丢掉。
    async fn send(&self, frame: Vec<u8>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };

        // 没配 `with_data_channel_send_buffer_limit`，所以 `send` 不会阻塞
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

/// 把一条 DataChannel 的事件泵出去。
///
/// 本端创建的与从 `on_data_channel` 拿到的都走这里，两条路径的行为必须一致。
fn pump(channel: Arc<dyn DataChannel>, driver: mpsc::UnboundedSender<Input>) {
    tokio::spawn(async move {
        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    let _ = driver.send(Input::Ready);
                }
                DataChannelEvent::OnMessage(message) => {
                    // 所有应用数据都走 Binary（与中继那条路一致）
                    if !message.is_string {
                        let _ = driver.send(Input::Inbound(message.data.to_vec()));
                    }
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

        let mut a_open = false;
        let mut b_open = false;

        // 把两条腿的信令互相喂过去，直到两边都报告通道打开
        let connected = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    event = events_a.next() => match event {
                        Some(P2pEvent::Signal(signal)) => link_b.handle_signal(signal),
                        Some(P2pEvent::ChannelOpen) => a_open = true,
                        _ => {}
                    },
                    event = events_b.next() => match event {
                        Some(P2pEvent::Signal(signal)) => link_a.handle_signal(signal),
                        Some(P2pEvent::ChannelOpen) => b_open = true,
                        _ => {}
                    },
                }

                if a_open && b_open {
                    return;
                }
            }
        })
        .await;

        assert!(
            connected.is_ok(),
            "30 秒内没有打通：a_open={a_open} b_open={b_open}"
        );

        // 通道是双向的：A 上发一帧，B 应该原样收到（`pet-state` 走的正是这条路）
        link_a.send(vec![1, 2, 3]);

        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events_b.next().await {
                    Some(P2pEvent::Inbound(bytes)) => return Some(bytes),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await;

        assert_eq!(received, Ok(Some(vec![1, 2, 3])));
    }
}

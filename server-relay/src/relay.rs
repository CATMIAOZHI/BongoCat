//! 会话层：一对连接、令牌桶限流、A ↔ B 转发、上下线控制帧。
//!
//! 行为逐条对齐 `server-cloudflare/src/pair.ts` 的 Durable Object：顶替顺序、
//! 「顶替之后还剩几个对端」的判定、`replaced` 过滤掉的伪离线通知，全部保持一样。

use std::collections::HashMap;
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

use crate::http::{write_upgrade, RequestHead};
use crate::protocol::{
    self, close_code, is_known_frame_kind, Limits, ServerFrame, FRAME_HEADER_SIZE,
    FRAME_KIND_TRANSFER_CHUNK, LAST_SEEN_WRITE_INTERVAL_MS, MAX_BINARY_FRAME_SIZE, PAIR_SIZE,
};

struct Peer {
    id: u64,
    device_id: String,
    sender: mpsc::Sender<Message>,
    last_seen: Instant,
    /// 这条连接被移出注册表时（顶替 / 停摆被摘）自动失效的信号，读循环据此退出。
    ///
    /// 只为了它的 `Drop` 存在：注册表是「这条连接还算数」的唯一真相，一旦摘牌，
    /// 读循环必须跟着结束，否则 socket 会一直挂着——注册表说它离线，它却还在把
    /// 帧转给对方，两边都不会自愈。
    #[allow(dead_code)]
    ejected: oneshot::Sender<()>,
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
    frames: f64,
    chunks: f64,
    bytes: f64,
    updated_at: Instant,
}

impl Bucket {
    fn full(limits: Limits, now: Instant) -> Self {
        Self {
            frames: limits.frames_per_second,
            chunks: limits.chunks_per_second,
            bytes: limits.bytes_per_second,
            updated_at: now,
        }
    }

    fn refill(&mut self, limits: Limits, now: Instant) {
        let elapsed = now.duration_since(self.updated_at).as_secs_f64();

        self.updated_at = now;
        self.frames =
            (self.frames + elapsed * limits.frames_per_second).min(limits.frames_per_second);
        self.chunks =
            (self.chunks + elapsed * limits.chunks_per_second).min(limits.chunks_per_second);
        self.bytes = (self.bytes + elapsed * limits.bytes_per_second).min(limits.bytes_per_second);
    }

    /// 扣掉本次额度；不够就返回 false（连接随之关闭，所以负值不必回滚，与 CF 版一致）
    fn take(&mut self, limits: Limits, now: Instant, frames: f64, chunks: f64, bytes: f64) -> bool {
        self.refill(limits, now);

        self.frames -= frames;
        self.chunks -= chunks;
        self.bytes -= bytes;

        self.frames >= 0.0 && self.chunks >= 0.0 && self.bytes >= 0.0
    }
}

#[derive(Default)]
struct State {
    peers: Vec<Peer>,
    buckets: HashMap<u64, Bucket>,
}

/// `admit` 的结果
#[derive(Debug)]
enum Admit {
    Accepted {
        id: u64,
        peer_online: bool,
        ejected: oneshot::Receiver<()>,
    },
    Full,
}

pub struct Relay {
    limits: Limits,
    stale_after: Duration,
    ice_servers: Option<serde_json::Value>,
    next_id: AtomicU64,
    state: Mutex<State>,
}

impl Relay {
    pub fn new(
        limits: Limits,
        stale_after: Duration,
        ice_servers: Option<serde_json::Value>,
    ) -> Arc<Self> {
        Arc::new(Self {
            limits,
            stale_after,
            ice_servers,
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        })
    }

    /// 完成握手、登记连接、跑读循环，直到这条连接结束。
    ///
    /// `head` 是已经被读掉的请求头（见 `http.rs`），这里不再重新解析。
    pub async fn serve(
        self: Arc<Self>,
        stream: TcpStream,
        head: RequestHead,
        device_id: String,
    ) -> Result<(), String> {
        let Some(key) = head.header("sec-websocket-key") else {
            return Err("缺少 Sec-WebSocket-Key".into());
        };

        let accept_key = derive_accept_key(key.trim().as_bytes());
        let mut stream = stream;

        write_upgrade(&mut stream, &accept_key)
            .await
            .map_err(|error| error.to_string())?;

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
        // 客户端才能把 4003 翻译成「配对已满」而不是一次普通连接失败）。
        let (id, mut ejected) = match self.admit(&device_id, &sender).await {
            Admit::Full => {
                let _ = websocket
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
                    limits: self.limits,
                    ice_servers: self.ice_servers.clone(),
                }
                .to_json();

                // 此刻还没有 writer 任务，独占 socket，直接发
                if let Err(error) = websocket.send(Message::Text(welcome.into())).await {
                    // 已经登记进注册表了，必须先摘牌：否则会留下一个「幽灵对端」
                    // ——占着配对位，还让对方一直以为它在线上。
                    self.drop_peer(id).await;

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

        loop {
            let item = tokio::select! {
                // 注册表里已经没有这条连接了：读循环必须跟着结束，否则 socket 会一直
                // 挂着（见 `Peer::ejected`）。
                _ = &mut ejected => break,
                item = stream_half.next() => item,
            };

            let Some(item) = item else { break };

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

                    self.touch(id).await;
                    self.forward(id, Message::Binary(bytes)).await;
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
        self.drop_peer(id).await;

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

    /// 决定新连接能不能进来，并处理顶替。返回值里的 `peer_online` 用于 welcome。
    async fn admit(&self, device_id: &str, sender: &mpsc::Sender<Message>) -> Admit {
        let mut state = self.state.lock().await;
        let now = Instant::now();

        let mine: Vec<u64> = state
            .peers
            .iter()
            .filter(|peer| peer.device_id == device_id)
            .map(|peer| peer.id)
            .collect();
        let others: Vec<u64> = state
            .peers
            .iter()
            .filter(|peer| peer.device_id != device_id)
            .map(|peer| peer.id)
            .collect();

        // 先算出「顶替之后还剩几个对端」，再决定是否动手关连接：先关再拒绝会把发起方
        // 自己原来的连接也关掉，让本来能恢复的情况变成完全连不上。
        let stale = if others.len() >= PAIR_SIZE {
            state
                .peers
                .iter()
                .find(|peer| {
                    peer.device_id != device_id
                        && now.duration_since(peer.last_seen) > self.stale_after
                })
                .map(|peer| peer.id)
        } else {
            None
        };

        let remaining = others.iter().filter(|id| Some(**id) != stale).count();

        if remaining >= PAIR_SIZE {
            // 持有同一个 PAIR_AUTH_TOKEN 的第三方无法进入：这是体验约束，不是安全边界（R9）
            return Admit::Full;
        }

        // 同一 deviceId 重连（例如切换网络）：关掉旧连接再接受新连接
        for id in mine {
            close_peer(
                &mut state,
                id,
                close_code::REPLACED,
                "replaced by a newer connection",
            );
        }

        if let Some(id) = stale {
            let stale_device_id = state
                .peers
                .iter()
                .find(|peer| peer.id == id)
                .map(|peer| peer.device_id.clone());

            close_peer(
                &mut state,
                id,
                close_code::STALE,
                "stale connection replaced",
            );

            // CF 版靠被顶替连接的 close 事件补这条离线通知（`announceOffline` 只过滤
            // 同一 deviceId 的伪通知，不排除 4004），所以这里也要补，否则同一个场景
            // 在两侧会发出不同的控制帧。放在新连接上线**之前**：存活方看到的是
            // 「旧的离线 → 新的上线」，终态仍然是在线（CF 靠事件时序拿到的是反过来的
            // 顺序，反而会以「离线」收尾）。
            if let Some(stale_device_id) = stale_device_id {
                announce_offline(&mut state, id, stale_device_id);
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (ejected, ejected_receiver) = oneshot::channel();

        state.buckets.insert(id, Bucket::full(self.limits, now));
        state.peers.push(Peer {
            id,
            device_id: device_id.to_string(),
            sender: sender.clone(),
            last_seen: now,
            ejected,
        });

        broadcast(
            &mut state,
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

    async fn drop_peer(&self, id: u64) {
        let mut state = self.state.lock().await;

        let Some(index) = state.peers.iter().position(|peer| peer.id == id) else {
            return;
        };

        let peer = state.peers.remove(index);

        state.buckets.remove(&id);

        announce_offline(&mut state, id, peer.device_id.clone());
    }

    /// 最后活动时间最多每 10 秒更新一次，避免高频写（顶替判定只需要粗粒度）
    async fn touch(&self, id: u64) {
        let mut state = self.state.lock().await;
        let now = Instant::now();

        if let Some(peer) = state.peers.iter_mut().find(|peer| peer.id == id) {
            if now.duration_since(peer.last_seen)
                >= Duration::from_millis(LAST_SEEN_WRITE_INTERVAL_MS)
            {
                peer.last_seen = now;
            }
        }
    }

    async fn allow(&self, id: u64, frames: f64, chunks: f64, bytes: f64) -> bool {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        // 已经不在注册表里的连接不再有桶：`entry().or_insert_with()` 会把桶重建出来，
        // 被摘掉的对端再发一帧就永久留下一条残留。这里直接拒绝，让读循环退出。
        let Some(bucket) = state.buckets.get_mut(&id) else {
            return false;
        };

        bucket.take(self.limits, now, frames, chunks, bytes)
    }

    /// 只转发给对端，不回发给发送者。
    ///
    /// 队列是有限的：对端消费不过来时这里会 await 等空位（反压回发送方的 TCP），
    /// 而不是把帧堆在内存里。真被拖过 `FORWARD_TIMEOUT` 就把那个对端摘掉——一条
    /// 停摆的连接不该拖死整台中继。
    async fn forward(&self, from: u64, message: Message) {
        let targets: Vec<(u64, mpsc::Sender<Message>)> = {
            let state = self.state.lock().await;

            state
                .peers
                .iter()
                .filter(|peer| peer.id != from)
                .map(|peer| (peer.id, peer.sender.clone()))
                .collect()
        };

        for (id, sender) in targets {
            match tokio::time::timeout(FORWARD_TIMEOUT, sender.send(message.clone())).await {
                Ok(Ok(())) => {}
                _ => {
                    // 队列已满且超时 / channel 已关闭：这个对端已经停摆
                    self.drop_peer(id).await;
                }
            }
        }
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

/// 广播「某台设备离线了」。
///
/// 同一 deviceId 的新连接已经在注册表里时（顶替重连）这条是伪广播：对端会看到
/// 「上线 → 下线」，最终以为对方离线，所以要过滤掉。CF 版的 `announceOffline` 是
/// 同一套规则（靠 close 事件晚于 accept 达到同样效果）。
fn announce_offline(state: &mut State, except: u64, device_id: String) {
    if is_false_offline(state, &device_id) {
        return;
    }

    broadcast(
        state,
        except,
        ServerFrame::Peer {
            online: false,
            device_id,
        },
    );
}

/// 同一 deviceId 的连接还在注册表里时（顶替重连），这条离线通知是伪广播
fn is_false_offline(state: &State, device_id: &str) -> bool {
    state.peers.iter().any(|other| other.device_id == device_id)
}

/// 把一条控制帧广播给 `except` 之外的连接。
///
/// 用 `try_send` 而不是 `send().await`：这里在锁内，等空位会把整个会话层卡住。
/// 投不进去说明那条连接已经停摆（出站队列满），把它从注册表里摘掉——留在里面会让
/// 另一方永远以为它在线，而它其实什么也收不到。
fn broadcast(state: &mut State, except: u64, frame: ServerFrame) {
    let mut pending = vec![(except, frame.to_json())];

    while let Some((except, json)) = pending.pop() {
        let mut stalled = Vec::new();

        for peer in state.peers.iter().filter(|peer| peer.id != except) {
            if peer
                .sender
                .try_send(Message::Text(json.clone().into()))
                .is_err()
            {
                stalled.push(peer.id);
            }
        }

        for id in stalled {
            let Some(index) = state.peers.iter().position(|peer| peer.id == id) else {
                continue;
            };

            let peer = state.peers.remove(index);

            state.buckets.remove(&id);

            // 它一条通知都没收到，所以剩下的人也必须知道它离线了。同一 deviceId 的
            // 新连接已经在列表里时（顶替重连），这条同样是伪通知，要一起过滤掉。
            if !is_false_offline(state, &peer.device_id) {
                pending.push((
                    id,
                    ServerFrame::Peer {
                        online: false,
                        device_id: peer.device_id,
                    }
                    .to_json(),
                ));
            }
        }
    }
}

/// 顶替一个已经登记的连接：移出注册表并把关闭帧交给它的 writer
fn close_peer(state: &mut State, id: u64, code: u16, reason: &str) {
    let Some(index) = state.peers.iter().position(|peer| peer.id == id) else {
        return;
    };

    let peer = state.peers.remove(index);

    state.buckets.remove(&id);
    let _ = peer
        .sender
        .try_send(Message::Close(Some(close_frame(code, reason))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot::error::TryRecvError;

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
        while bucket.take(limits, start, 1.0, 0.0, frame) {
            admitted += 1;
        }

        assert_eq!(admitted, 30);

        // 过一秒补满。注意上一次超限把桶扣成了 -1（与 CF 版一致：先扣再判，连接随即
        // 关闭所以不回滚），所以补满后能放行的是 29 帧。
        let later = start + Duration::from_secs(1);
        let mut admitted_after_refill = 0;

        while bucket.take(limits, later, 1.0, 0.0, frame) {
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
            assert!(bucket.take(limits, start, 1.0, 1.0, chunk));
        }

        // 第 21 个 chunk 会被分片额度拦住
        assert!(!bucket.take(limits, start, 1.0, 1.0, chunk));
    }

    #[test]
    fn the_byte_bucket_is_twelve_mebibytes() {
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::full(limits, start);
        let half = limits.bytes_per_second / 2.0;

        assert!(bucket.take(limits, start, 0.0, 0.0, half));
        assert!(bucket.take(limits, start, 0.0, 0.0, half));
        assert!(!bucket.take(limits, start, 0.0, 0.0, 1.0));
    }

    #[tokio::test]
    async fn admits_two_distinct_devices_and_rejects_the_third() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a, mut receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_c, _receiver_c) = mpsc::channel(OUTBOUND_QUEUE);

        let Admit::Accepted { peer_online, .. } = relay.admit("a", &sender_a).await else {
            panic!("第一个连接应当被接受");
        };

        assert!(!peer_online, "第一个人进来时对端还没上线");

        let Admit::Accepted { peer_online, .. } = relay.admit("b", &sender_b).await else {
            panic!("第二个连接应当被接受");
        };

        assert!(peer_online, "第二个人进来时对方已在线");

        let announced = drain(&mut receiver_a);
        let json: serde_json::Value =
            serde_json::from_str(announced[0].to_text().unwrap()).unwrap();

        assert_eq!(json["type"], "server.peer");
        assert_eq!(json["online"], true);
        assert_eq!(json["deviceId"], "b");
        assert!(drain(&mut receiver_b).is_empty(), "上线通知不能回发给本人");

        assert!(matches!(relay.admit("c", &sender_c).await, Admit::Full));
    }

    #[tokio::test]
    async fn reconnecting_with_the_same_device_id_replaces_without_a_false_offline() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a1, mut receiver_a1) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_a2, mut receiver_a2) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);

        assert!(matches!(
            relay.admit("a", &sender_a1).await,
            Admit::Accepted { .. }
        ));
        assert!(matches!(
            relay.admit("b", &sender_b).await,
            Admit::Accepted { .. }
        ));

        let Admit::Accepted { peer_online, .. } = relay.admit("a", &sender_a2).await else {
            panic!("同一个 deviceId 重连不能被当成第三个人");
        };

        assert!(peer_online);
        assert_eq!(
            drain(&mut receiver_a1)
                .iter()
                .filter_map(close_code_of)
                .next(),
            Some(close_code::REPLACED)
        );
        assert!(drain(&mut receiver_a2).is_empty(), "新连接不该收到关闭帧");

        // B 只看到 A 上线的通知，不该看到「A 下线」
        let seen = drain(&mut receiver_b);

        assert_eq!(seen.iter().filter_map(close_code_of).count(), 0);
        assert_eq!(seen.len(), 1);
    }

    #[tokio::test]
    async fn a_stale_peer_is_evicted_with_4004() {
        let relay = Relay::new(Limits::default(), Duration::ZERO, None);
        let (sender_a, mut receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_c, _receiver_c) = mpsc::channel(OUTBOUND_QUEUE);

        assert!(matches!(
            relay.admit("a", &sender_a).await,
            Admit::Accepted { .. }
        ));
        assert!(matches!(
            relay.admit("b", &sender_b).await,
            Admit::Accepted { .. }
        ));

        drain(&mut receiver_a);
        drain(&mut receiver_b);

        tokio::time::sleep(Duration::from_millis(5)).await;

        let Admit::Accepted { peer_online, .. } = relay.admit("c", &sender_c).await else {
            panic!("陈旧连接应当被顶替，而不是让第三个人被拒");
        };

        assert!(peer_online, "顶替之后还剩一个对端");

        // 顶替的是列表里第一个陈旧的连接（先连进来的那个）
        assert_eq!(
            drain(&mut receiver_a)
                .iter()
                .filter_map(close_code_of)
                .next(),
            Some(close_code::STALE)
        );
        // 活下来的那个收到「a 离线」再收到「c 上线」，而不是关闭帧。
        //
        // 离线这条是 CF 版也有的（它的 `announceOffline` 只过滤同一 deviceId 的伪通知，
        // 4004 被顶替的那台不属于新连接的 deviceId）；顺序放在上线之前，存活方最后的
        // 判断才是「在线」。
        let seen: Vec<serde_json::Value> = drain(&mut receiver_b)
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
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a, mut receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, _receiver_b) = mpsc::channel(OUTBOUND_QUEUE);

        assert!(matches!(
            relay.admit("a", &sender_a).await,
            Admit::Accepted { .. }
        ));

        let Admit::Accepted { id, .. } = relay.admit("b", &sender_b).await else {
            panic!("第二个连接应当被接受");
        };

        drain(&mut receiver_a);
        relay.drop_peer(id).await;

        let seen = drain(&mut receiver_a);
        let json: serde_json::Value = serde_json::from_str(seen[0].to_text().unwrap()).unwrap();

        assert_eq!(json["type"], "server.peer");
        assert_eq!(json["online"], false);
        assert_eq!(json["deviceId"], "b");
    }

    #[tokio::test]
    async fn the_rate_limit_is_per_socket() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a, _receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, _receiver_b) = mpsc::channel(OUTBOUND_QUEUE);

        let Admit::Accepted { id: id_a, .. } = relay.admit("a", &sender_a).await else {
            panic!("A 应当被接受");
        };
        let Admit::Accepted { id: id_b, .. } = relay.admit("b", &sender_b).await else {
            panic!("B 应当被接受");
        };

        for _ in 0..30 {
            assert!(relay.allow(id_a, 1.0, 0.0, 64.0).await);
        }
        assert!(!relay.allow(id_a, 1.0, 0.0, 64.0).await);

        // 另一个 socket 有自己的桶
        assert!(relay.allow(id_b, 1.0, 0.0, 64.0).await);

        // 已经摘掉的连接不再有桶，也不会被 `entry().or_insert_with()` 重新造出来
        relay.drop_peer(id_b).await;
        assert!(!relay.allow(id_b, 1.0, 0.0, 64.0).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_peer_is_ejected_after_the_forward_timeout() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        // A 的接收端一直活着但不消费：队列会满，`send` 会一直等空位
        let (sender_a, mut receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_c, _receiver_c) = mpsc::channel(OUTBOUND_QUEUE);

        let Admit::Accepted { mut ejected, .. } = relay.admit("a", &sender_a).await else {
            panic!("A 应当被接受");
        };
        let Admit::Accepted { id: id_b, .. } = relay.admit("b", &sender_b).await else {
            panic!("B 应当被接受");
        };

        drain(&mut receiver_a);
        drain(&mut receiver_b);

        let frame = Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into());

        for _ in 0..OUTBOUND_QUEUE {
            relay.forward(id_b, frame.clone()).await;
        }

        // 队列满了，这一帧要等满 FORWARD_TIMEOUT 才会被放弃（`start_paused` 会自动
        // 把时钟推到那个定时器）
        relay.forward(id_b, frame).await;

        // 摘牌之后 A 的读循环必须能收到信号，否则它的 socket 会一直挂着
        assert!(matches!(ejected.try_recv(), Err(TryRecvError::Closed)));

        // 配对位也空出来了：第三个人进来不该被当成第三人
        assert!(matches!(
            relay.admit("c", &sender_c).await,
            Admit::Accepted { .. }
        ));

        // B 也收到了「A 离线」，不会一直以为对方在线
        assert!(announced_offline(&mut receiver_b, "a"));
    }

    #[tokio::test]
    async fn an_observer_that_cannot_be_reached_is_evicted() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a, mut receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_c, _receiver_c) = mpsc::channel(OUTBOUND_QUEUE);

        let Admit::Accepted { mut ejected, .. } = relay.admit("a", &sender_a).await else {
            panic!("A 应当被接受");
        };
        let Admit::Accepted { id: id_b, .. } = relay.admit("b", &sender_b).await else {
            panic!("B 应当被接受");
        };

        drain(&mut receiver_a);
        drain(&mut receiver_b);

        let frame = Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into());

        for _ in 0..OUTBOUND_QUEUE {
            relay.forward(id_b, frame.clone()).await;
        }

        // B 断开时的离线通知投不进 A（队列满）：A 必须被摘掉，而不是被静默跳过
        relay.drop_peer(id_b).await;

        assert!(matches!(ejected.try_recv(), Err(TryRecvError::Closed)));
        assert!(matches!(
            relay.admit("c", &sender_c).await,
            Admit::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn forwarding_to_a_closed_queue_drops_the_stalled_peer() {
        let relay = Relay::new(Limits::default(), Duration::from_secs(120), None);
        let (sender_a, receiver_a) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_b, mut receiver_b) = mpsc::channel(OUTBOUND_QUEUE);
        let (sender_c, _receiver_c) = mpsc::channel(OUTBOUND_QUEUE);

        let Admit::Accepted { id: id_a, .. } = relay.admit("a", &sender_a).await else {
            panic!("A 应当被接受");
        };
        let Admit::Accepted { id: id_b, .. } = relay.admit("b", &sender_b).await else {
            panic!("B 应当被接受");
        };

        // 让 A 的连接「死掉」：接收端被丢掉，转发时会立刻发现 channel 关闭
        drop(receiver_a);
        drain(&mut receiver_b);

        relay
            .forward(id_b, Message::Binary(vec![0u8; FRAME_HEADER_SIZE].into()))
            .await;

        // A 被摘掉后，配对位空出来了：第三个人进来不该被当成第三人
        assert!(matches!(
            relay.admit("c", &sender_c).await,
            Admit::Accepted { .. }
        ));
        assert_ne!(id_a, id_b);

        // 而且 B 收到了「A 离线」，不会一直以为对方在线
        assert!(announced_offline(&mut receiver_b, "a"));
    }
}

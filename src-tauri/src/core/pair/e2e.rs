//! 针对真实中继的端到端测试。
//!
//! 这些用例需要一个正在运行的 relay（本地 `pnpm dev` 或已部署的 Worker），
//! 所以默认被 `#[ignore]` 跳过。运行方式：
//!
//! ```powershell
//! $env:BONGO_PAIR_E2E_RELAY = "http://127.0.0.1:8787"
//! $env:BONGO_PAIR_E2E_SECRET = "<配对密码（两端填同一个值）>"
//! $env:BONGO_PAIR_HEARTBEAT_SECS = "2"
//! cargo test --manifest-path src-tauri/Cargo.toml --lib pair::e2e -- --ignored --nocapture
//! ```
//!
//! 除了 `two_rooms_share_one_relay_without_crossing`（它需要多会话版本的自建中继，
//! 即 `PAIR_MAX_SESSIONS >= 2`，并且要求本机能打通 P2P——它要等四条 DataChannel 各自
//! 立起来）之外，其余用例在 Cloudflare 版与自建版上都该通过。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use super::client;
use super::crypto::{self};
use super::history::{MessageStatus, PairHistory};
use super::history::{MessageKind, NewAttachment, NewMessage};
use super::manager::OutgoingRequest;
use super::transfer::{TransferStore, sha256_file};
use super::manager::{
    EVENT_CONNECTION_CHANGED, EVENT_MESSAGE_RECEIVED, EVENT_MESSAGE_UPDATED, EVENT_PEER_CHANGED,
    EVENT_PET_STATE, EVENT_PRESENCE, PairConnectionState, PairManager,
};
use super::protocol::{
    FrameKind, PetSnapshot, PresencePayload, PresenceState, TransferKind, message_type,
};

/// 记录所有事件的测试用 sink
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, Value)>>,
}

/// 每个 e2e 客户端一个内存聊天库：端到端用例不碰真实磁盘
fn memory_history() -> Arc<PairHistory> {
    Arc::new(PairHistory::in_memory().expect("内存聊天库"))
}

/// 附件目录也用临时目录：端到端用例不往用户的数据目录里写文件
fn memory_store() -> TransferStore {
    let root = std::env::temp_dir().join(format!("bongo-cat-pair-e2e-{}", uuid::Uuid::new_v4()));

    TransferStore::new(root)
}

impl RecordingSink {
    fn status_events(&self, event: &str) -> Vec<Value> {
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        events
            .iter()
            .filter(|(name, _)| name == event)
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    fn last_state(&self) -> Option<PairConnectionState> {
        let payload = self.status_events(EVENT_CONNECTION_CHANGED).pop()?;

        serde_json::from_value(payload.get("state")?.clone()).ok()
    }

    /// 连接失败的原因，方便断言失败时直接看到
    fn errors(&self) -> Vec<String> {
        self.status_events(super::manager::EVENT_ERROR)
            .iter()
            .filter_map(|payload| payload.get("message")?.as_str().map(str::to_string))
            .collect()
    }

    fn presence_events(&self) -> Vec<Value> {
        self.status_events(EVENT_PRESENCE)
    }
}

impl super::manager::PairEventSink for RecordingSink {
    fn emit(&self, event: &str, payload: Value) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        events.push((event.to_string(), payload));
    }
}

fn e2e_config() -> Option<(String, String)> {
    let relay = std::env::var("BONGO_PAIR_E2E_RELAY").ok()?;
    let secret = std::env::var("BONGO_PAIR_E2E_SECRET").ok()?;

    if relay.trim().is_empty() || secret.trim().is_empty() {
        return None;
    }

    Some((relay, secret))
}

/// R36：这一版自建中继会要求服务器密码，跑 e2e 时用 `BONGO_PAIR_E2E_SERVER_PASSWORD`
/// 传进来。没设就不带 `X-Bongo-Server` 头——旧的单会话中继与 Cloudflare 版仍然是
/// 同一批用例跑得通的（这正是「不改协议也能兼容」的证据）。
fn e2e_server_password() -> Option<String> {
    std::env::var("BONGO_PAIR_E2E_SERVER_PASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 服务器密码 → 升级头里的凭据
fn e2e_server_token() -> Option<String> {
    e2e_server_password().map(|password| super::crypto::derive_server_token(&password))
}

/// 端到端用例必须串行执行。
///
/// 它们共用同一个会话（同一个 `BONGO_PAIR_E2E_SECRET`），并行时第二个用例的 deviceId
/// 会被当成同一会话里的第三人并以 4003 拒绝。Cloudflare 版一个部署永远只有一对用户，
/// 所以那边更是必须串行。
fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());

    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn wait_for<F: Fn() -> bool>(predicate: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    predicate()
}

/// 挡在真中继前面的字节计数器（§10 的单机版「服务器转发量」观测）。
///
/// 只做 TCP 转发与计数，**不解析 WebSocket**：所以两个方向的字节数包含握手、心跳与帧头
/// 开销——用例要的正是「服务器到底经手了多少字节」的上界。
struct ProxyBytes {
    /// 客户端 -> 中继（中继真正收到并转发的量）
    to_relay: AtomicU64,
    /// 中继 -> 客户端
    to_client: AtomicU64,
}

impl ProxyBytes {
    fn snapshot(&self) -> (u64, u64) {
        (
            self.to_relay.load(Ordering::Relaxed),
            self.to_client.load(Ordering::Relaxed),
        )
    }
}

enum Direction {
    ToRelay,
    ToClient,
}

/// 在中继前面起一个计数代理，返回它的地址与计数器。
///
/// 客户端把它当成中继地址填进去即可：中继只按路径（`/ws`）、鉴权头与协议版本判断
/// 要不要升级，不校验 Host，所以中间多一跳不影响握手。
async fn counting_proxy(upstream: SocketAddr) -> (SocketAddr, Arc<ProxyBytes>) {
    let bytes = Arc::new(ProxyBytes {
        to_relay: AtomicU64::new(0),
        to_client: AtomicU64::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("计数代理的监听端口");
    let address = listener.local_addr().expect("计数代理的地址");
    let counts = Arc::clone(&bytes);

    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                break;
            };
            let Ok(relay) = tokio::net::TcpStream::connect(upstream).await else {
                // 连不上中继只丢这一条连接：重连由客户端自己退避
                continue;
            };
            let counts = Arc::clone(&counts);

            tokio::spawn(async move {
                let (client_read, client_write) = client.into_split();
                let (relay_read, relay_write) = relay.into_split();

                tokio::join!(
                    pump(
                        client_read,
                        relay_write,
                        Arc::clone(&counts),
                        Direction::ToRelay
                    ),
                    pump(relay_read, client_write, counts, Direction::ToClient),
                );
            });
        }
    });

    (address, bytes)
}

async fn pump<R, W>(mut reader: R, mut writer: W, counts: Arc<ProxyBytes>, direction: Direction)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; 16 * 1024];

    loop {
        let Ok(read) = reader.read(&mut buffer).await else {
            break;
        };

        if read == 0 {
            break;
        }

        if writer.write_all(&buffer[..read]).await.is_err() {
            break;
        }

        match direction {
            Direction::ToRelay => counts.to_relay.fetch_add(read as u64, Ordering::Relaxed),
            Direction::ToClient => counts.to_client.fetch_add(read as u64, Ordering::Relaxed),
        };
    }

    // 一边结束就把另一半的写方向也关掉，让对端一起收尾
    let _ = writer.shutdown().await;
}

/// 两个客户端通过真实中继交换一条加密的 Presence 消息
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn two_clients_exchange_encrypted_presence() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let manager_a = Arc::new(PairManager::new(
        "e2e-a".into(),
        sink_a.clone(),
        memory_history(),
        memory_store(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-b".into(),
        sink_b.clone(),
        memory_history(),
        memory_store(),
    ));

    manager_a.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    manager_a
        .send(
            FrameKind::Presence,
            message_type::PRESENCE,
            serde_json::to_value(PresencePayload {
                state: PresenceState::Away,
                message: Some("去吃饭啦".into()),
                display_name: Some("A".into()),
            })
            .unwrap(),
        )
        .unwrap();

    // B 的 manager 是通过真实中继收到并解密的，事件就是解密后的载荷
    let arrived = wait_for(
        || !sink_b.presence_events().is_empty(),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        arrived,
        "B 没有收到 A 的 presence：errors={:?}",
        sink_b.errors()
    );

    let payload = sink_b.presence_events().into_iter().next().unwrap();

    assert_eq!(payload["state"], "away");
    assert_eq!(payload["message"], "去吃饭啦");
    assert_eq!(payload["displayName"], "A");

    // 中继只转发给对端，发送方不该收到自己的消息
    assert!(
        sink_a.presence_events().is_empty(),
        "发送方收到了自己的消息：{:?}",
        sink_a.presence_events()
    );

    manager_a.disconnect();
    manager_b.disconnect();
}

/// §32：一套服务器上的两组会话互不可见。
///
/// 四个 `PairManager`：A1/A2 用 `BONGO_PAIR_E2E_SECRET`，B1/B2 用另一个固定密钥
/// （`0xAB` × 32 的 base64url，与前者不同即可——用例不需要第二个环境变量）。
/// 断言两件事：自己那组照常收发；**另一组一条都收不到**（负向断言）。
///
/// 需要**多会话版本**的中继（`PAIR_MAX_SESSIONS >= 2`）**且本机能打通 P2P**：
/// 旧版一个部署只服务一对用户，第二组会被当成第三台设备拒掉；而
/// 后半段要等四条 DataChannel 各自立起来（§32），打不通的机器上会以「两组的
/// DataChannel 没有各自打通」失败——那是环境问题，不是串房回归。
#[tokio::test]
#[ignore = "需要多会话版本的 relay 且本机 P2P 可打通，见文件头说明"]
async fn two_rooms_share_one_relay_without_crossing() {
    /// `0xAB` × 32 的 base64url 无填充：与 `BONGO_PAIR_E2E_SECRET` 不同，
    /// 所以派生出的 `ROOM_ID` 与 `AUTH_TOKEN` 都不同
    const SECRET_B: &str = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s";

    let Some((relay, secret_a)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    // 两组各两个人，四个独立的 manager 与 sink
    let mut rooms = Vec::new();

    for (device_id, secret) in [
        ("e2e-room-a1", secret_a.as_str()),
        ("e2e-room-a2", secret_a.as_str()),
        ("e2e-room-b1", SECRET_B),
        ("e2e-room-b2", SECRET_B),
    ] {
        let sink = Arc::new(RecordingSink::default());
        let manager = Arc::new(PairManager::new(
            device_id.into(),
            sink.clone(),
            memory_history(),
            memory_store(),
        ));

        manager.start(&relay, Some(secret), e2e_server_password().as_deref()).unwrap();

        rooms.push((device_id, manager, sink));
    }

    // 四个都连上、而且各自看到自己那组的对端在线：两组会话都在同一个中继上活着
    let both_rooms_up = wait_for(
        || {
            rooms
                .iter()
                .all(|(_, _, sink)| sink.last_state() == Some(PairConnectionState::Connected))
        },
        Duration::from_secs(20),
    )
    .await;

    assert!(
        both_rooms_up,
        "两组会话没有同时进入 Connected：{:?}",
        rooms
            .iter()
            .map(|(device_id, _, sink)| (*device_id, sink.last_state(), sink.errors()))
            .collect::<Vec<_>>()
    );

    let send_presence = |label: &str, manager: &Arc<PairManager>| {
        manager
            .send(
                FrameKind::Presence,
                message_type::PRESENCE,
                serde_json::to_value(PresencePayload {
                    state: PresenceState::Away,
                    message: Some(label.into()),
                    display_name: Some(label.into()),
                })
                .unwrap(),
            )
            .unwrap()
    };

    // A 组先说话：只有同组的 A2 该收到
    send_presence("A", &rooms[0].1);

    let arrived = wait_for(
        || !rooms[1].2.presence_events().is_empty(),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        arrived,
        "同组的 A2 没收到 A1 的 presence：errors={:?}",
        rooms[1].2.errors()
    );

    assert_eq!(rooms[1].2.presence_events()[0]["message"], "A");

    // B 组再说一句：同样只有同组的 B2 该收到
    send_presence("B", &rooms[2].1);

    let arrived = wait_for(
        || !rooms[3].2.presence_events().is_empty(),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        arrived,
        "同组的 B2 没收到 B1 的 presence：errors={:?}",
        rooms[3].2.errors()
    );

    assert_eq!(rooms[3].2.presence_events()[0]["message"], "B");

    // §32 的后半段：两组各自的 DataChannel。信令走的是同一条中继，如果 ICE 串了房，
    // 这两条腿根本立不起来（或立到别人的设备上），所以「四条腿都 connected」本身
    // 就是「信令没有串房」的证据。
    let both_channels_up = wait_for(
        || {
            rooms
                .iter()
                .all(|(_, _, sink)| last_p2p(sink).as_deref() == Some("connected"))
        },
        Duration::from_secs(30),
    )
    .await;

    assert!(
        both_channels_up,
        "两组的 DataChannel 没有各自打通：{:?}",
        rooms
            .iter()
            .map(|(device_id, _, sink)| (*device_id, last_p2p(sink), sink.errors()))
            .collect::<Vec<_>>()
    );

    // 走 DC 的桌宠快照同样只在组内可见（A1 发、A2 收，B 组一条都不该有）
    rooms[0]
        .1
        .send_replaceable(
            FrameKind::PetState,
            message_type::PET_STATE,
            serde_json::to_value(PetSnapshot::default()).unwrap(),
        )
        .unwrap();

    let snapshot = wait_for(
        || !rooms[1].2.status_events(EVENT_PET_STATE).is_empty(),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        snapshot,
        "同组的 A2 没收到 A1 的桌宠快照：errors={:?}",
        rooms[1].2.errors()
    );

    // 所有「该发的」都发完了，再等一小会儿：串房是「本来该到、只是晚了几百微秒」的
    // 形态，让迟到的帧有机会到达，后面的负向断言才不是抢在它前面跑。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 负向断言之一：事件。每一帧都由**本会话的密钥**封着，所以串房投递到另一边时
    // 解不开——它进不了 presence / pet-state 事件，只会变成一条 error。所以事件计数
    // 必须和「错误计数」一起断言，否则「A 房的帧塞给 B 房」会被这组断言漏掉。
    let cross_room_errors = |device_id: &str, sink: &RecordingSink| {
        let errors: Vec<String> = sink
            .errors()
            .into_iter()
            .filter(|error| error.contains("解密失败") || error.contains("未知的帧类型"))
            .collect();

        assert!(
            errors.is_empty(),
            "{device_id} 收到了别的会话的帧（解不开/帧类型不认识）：{errors:?}"
        );
    };

    for (device_id, sink) in rooms.iter().map(|(device_id, _, sink)| (*device_id, sink)) {
        cross_room_errors(device_id, sink);
    }

    for (device_id, sink) in [(&rooms[0].0, &rooms[0].2), (&rooms[1].0, &rooms[1].2)] {
        let seen = sink.presence_events();

        assert_eq!(
            seen.len(),
            usize::from(*device_id == "e2e-room-a2"),
            "{device_id} 收到了不该收到的 presence：{seen:?}"
        );
    }

    assert!(
        rooms[2].2.status_events(EVENT_PET_STATE).is_empty()
            && rooms[3].2.status_events(EVENT_PET_STATE).is_empty(),
        "B 组收到了 A 组的桌宠快照：B1={:?} B2={:?}",
        rooms[2].2.status_events(EVENT_PET_STATE),
        rooms[3].2.status_events(EVENT_PET_STATE)
    );

    for (device_id, sink) in [(&rooms[2].0, &rooms[2].2), (&rooms[3].0, &rooms[3].2)] {
        let seen = sink.presence_events();

        assert_eq!(
            seen.len(),
            usize::from(*device_id == "e2e-room-b2"),
            "{device_id} 收到了不该收到的 presence：{seen:?}"
        );
    }

    for (_, manager, _) in rooms {
        manager.disconnect();
    }
}

/// 两个客户端通过真实中继互发文字消息（§31 / §32）。
///
/// 覆盖三段：在线的正常收发与 ack、对方离线时消息先在本地排队、对方回来后自动补发并变成
/// `delivered`。这段必须走真实中继，才说明「服务器不存内容、靠客户端补发」的约定成立。
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn two_clients_exchange_encrypted_chat_messages() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let history_a = memory_history();
    let history_b = memory_history();
    let manager_a = Arc::new(PairManager::new(
        "e2e-chat-a".into(),
        sink_a.clone(),
        Arc::clone(&history_a),
        memory_store(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-chat-b".into(),
        sink_b.clone(),
        Arc::clone(&history_b),
        memory_store(),
    ));

    manager_a.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    // A 发、B 入库并回 ack、A 的状态变成 delivered
    let sent = manager_a.send_chat("你好，吃了吗").unwrap();

    let stored = wait_for(
        || sink_b.status_events(EVENT_MESSAGE_RECEIVED).len() == 1,
        Duration::from_secs(10),
    )
    .await;

    assert!(stored, "B 没有收到文字消息：errors={:?}", sink_b.errors());

    let received = sink_b
        .status_events(EVENT_MESSAGE_RECEIVED)
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(received["id"], sent.id);
    assert_eq!(received["text"], "你好，吃了吗");
    assert_eq!(received["direction"], "incoming");

    let acked = wait_for(
        || {
            sink_a
                .status_events(EVENT_MESSAGE_UPDATED)
                .iter()
                .any(|payload| payload["status"] == "delivered")
        },
        Duration::from_secs(10),
    )
    .await;

    assert!(
        acked,
        "A 没有等到 ack：{:?}",
        sink_a.status_events(EVENT_MESSAGE_UPDATED)
    );

    // §32：B 离线时消息留在本地队列，B 回来后才补发
    manager_b.disconnect();

    let peer_gone = wait_for(
        || {
            sink_a
                .status_events(EVENT_PEER_CHANGED)
                .iter()
                .any(|payload| payload["online"] == false)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(peer_gone, "A 没有察觉 B 离线");

    let queued = manager_a.send_chat("回来叫我").unwrap();

    assert_ne!(queued.status, MessageStatus::Delivered);
    assert_eq!(
        sink_b.status_events(EVENT_MESSAGE_RECEIVED).len(),
        1,
        "B 离线期间不该收到消息"
    );

    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let resent = wait_for(
        || sink_b.status_events(EVENT_MESSAGE_RECEIVED).len() == 2,
        Duration::from_secs(15),
    )
    .await;

    assert!(resent, "B 回来后没有收到补发：errors={:?}", sink_b.errors());
    assert_eq!(
        sink_b.status_events(EVENT_MESSAGE_RECEIVED)[1]["id"],
        queued.id
    );

    let delivered = wait_for(
        || {
            history_a
                .find(&queued.id)
                .ok()
                .flatten()
                .map(|message| message.status)
                == Some(MessageStatus::Delivered)
        },
        Duration::from_secs(10),
    )
    .await;

    assert!(delivered, "补发的消息没有变成 delivered");

    manager_a.disconnect();
    manager_b.disconnect();
}

/// 离线积压后的一次性补发（R18）：中继限流是 30 帧/秒，客户端补发必须自己不越界，
/// 不能出现「连上就被 close 1008、ack 永远收不到」的空转。
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn offline_backlog_is_delivered_without_tripping_the_relay_limit() {
    /// 40 条会同时跨过客户端的突发额度（20）与中继的突发额度（30）
    const BACKLOG: usize = 40;

    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let history_a = memory_history();
    let history_b = memory_history();
    let manager_a = Arc::new(PairManager::new(
        "e2e-backlog-a".into(),
        sink_a.clone(),
        Arc::clone(&history_a),
        memory_store(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-backlog-b".into(),
        sink_b.clone(),
        Arc::clone(&history_b),
        memory_store(),
    ));

    manager_a.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    // B 下线，A 攒下 40 条只存在本地的离线消息
    manager_b.disconnect();

    let peer_gone = wait_for(
        || {
            sink_a
                .status_events(EVENT_PEER_CHANGED)
                .iter()
                .any(|payload| payload["online"] == false)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(peer_gone, "A 没有察觉 B 离线");

    let ids: Vec<String> = (0..BACKLOG)
        .map(|index| manager_a.send_chat(&format!("积压 #{index}")).unwrap().id)
        .collect();

    assert!(
        sink_b.status_events(EVENT_MESSAGE_RECEIVED).is_empty(),
        "B 离线期间不该收到消息"
    );

    // B 回来：A 补发 40 帧（20 帧突发 + 20 帧按 20/s），仍在中继 30 帧/秒的额度内
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let all_arrived = wait_for(
        || sink_b.status_events(EVENT_MESSAGE_RECEIVED).len() == BACKLOG,
        Duration::from_secs(30),
    )
    .await;

    assert!(
        all_arrived,
        "B 只收到 {} 条补发：errors={:?}",
        sink_b.status_events(EVENT_MESSAGE_RECEIVED).len(),
        sink_b.errors()
    );

    let received_ids: Vec<String> = sink_b
        .status_events(EVENT_MESSAGE_RECEIVED)
        .into_iter()
        .filter_map(|payload| payload["id"].as_str().map(str::to_string))
        .collect();

    assert_eq!(received_ids.len(), BACKLOG, "补发里出现了重复或缺失的消息");

    for id in &ids {
        assert!(received_ids.contains(id), "补发丢了消息 {id}");
    }

    let all_acked = wait_for(
        || {
            ids.iter().all(|id| {
                history_a
                    .find(id)
                    .ok()
                    .flatten()
                    .map(|message| message.status)
                    == Some(MessageStatus::Delivered)
            })
        },
        Duration::from_secs(20),
    )
    .await;

    assert!(
        all_acked,
        "补发的消息没有全部变成 delivered：errors={:?}",
        sink_a.errors()
    );

    let errors = sink_a.errors();

    assert!(
        !errors
            .iter()
            .any(|message| message.contains("1008") || message.contains("发送频率")),
        "补发被中继限流踢掉了：{errors:?}"
    );

    manager_a.disconnect();
    manager_b.disconnect();
}

/// 附件走真实中继（§81）：A 发 1.5 MiB、B 收下并校验 SHA-256，两边都是终态
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn two_clients_exchange_a_file_through_the_relay() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let store_a = memory_store();
    let store_b = memory_store();
    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let history_a = memory_history();
    let history_b = memory_history();
    let manager_a = Arc::new(PairManager::new(
        "e2e-file-a".into(),
        sink_a.clone(),
        Arc::clone(&history_a),
        store_a.clone(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-file-b".into(),
        sink_b.clone(),
        Arc::clone(&history_b),
        store_b.clone(),
    ));

    manager_a.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    // A 侧造一个 1.5 MiB 的附件（约 3 块）
    let payload: Vec<u8> = (0..(1024 * 1536)).map(|index| (index % 251) as u8).collect();
    let source = store_a.root().join("相册照片.bin");

    std::fs::create_dir_all(store_a.root()).unwrap();
    std::fs::write(&source, &payload).unwrap();

    let (sha256, size) = sha256_file(&source).unwrap();
    let message_id = "e2e-file-message".to_string();
    let attachment_id = "e2e-file-attachment".to_string();

    history_a
        .upsert_attachment(&NewAttachment {
            id: attachment_id.clone(),
            kind: MessageKind::File,
            original_name: Some("相册照片.bin".into()),
            mime: Some("application/octet-stream".into()),
            size: Some(size),
            sha256: Some(sha256.clone()),
            local_path: Some(source.to_string_lossy().to_string()),
            created_at: 0,
        })
        .unwrap();
    history_a
        .insert(&NewMessage::outgoing_attachment(
            message_id.clone(),
            MessageKind::File,
            attachment_id.clone(),
            0,
            history_a.epoch().unwrap(),
        ))
        .unwrap();

    manager_a
        .start_transfer(OutgoingRequest {
            transfer_id: super::manager::new_transfer_id(),
            message_id: message_id.clone(),
            attachment_id,
            kind: TransferKind::File,
            name: "相册照片.bin".into(),
            mime: "application/octet-stream".into(),
            size,
            sha256,
            path: source,
        })
        .unwrap();

    // B 收完并落盘
    let received = wait_for(
        || {
            history_b
                .find(&message_id)
                .ok()
                .flatten()
                .map(|message| message.status)
                == Some(MessageStatus::Received)
        },
        Duration::from_secs(30),
    )
    .await;

    assert!(
        received,
        "B 没有收下附件：errors={:?}",
        (sink_b.errors(), sink_a.errors())
    );

    let message = history_b.find(&message_id).unwrap().unwrap();
    let path = message
        .attachment
        .expect("B 的附件记录")
        .local_path
        .expect("B 的落盘路径");

    assert_eq!(std::fs::read(&path).unwrap(), payload);
    assert!(
        std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .len()
            > 20,
        "落盘名应当是 UUID"
    );

    // A 收到 verified 之后才是 delivered
    let delivered = wait_for(
        || {
            history_a
                .find(&message_id)
                .ok()
                .flatten()
                .map(|message| message.status)
                == Some(MessageStatus::Delivered)
        },
        Duration::from_secs(20),
    )
    .await;

    assert!(
        delivered,
        "A 的附件没有变成 delivered：errors={:?}",
        sink_a.errors()
    );

    // 三块数据在中继的 20 chunk/s 额度内，不该被限流踢掉
    let errors = sink_a.errors();

    assert!(
        !errors
            .iter()
            .any(|message| message.contains("1008") || message.contains("发送频率")),
        "附件传输被中继限流踢掉了：{errors:?}"
    );

    manager_a.disconnect();
    manager_b.disconnect();
}

/// 心跳期间连接保持存活（客户端发 WS ping，中继回 pong）
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn stays_connected_across_heartbeats() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let secret = crypto::decode_pair_secret(&secret_text).unwrap();
    let auth_token = crypto::derive_auth_token(&secret);
    let room_id = crypto::derive_room_id(&secret);

    let sink = Arc::new(RecordingSink::default());
    let manager = Arc::new(PairManager::new(
        "e2e-heartbeat".into(),
        sink.clone(),
        memory_history(),
        memory_store(),
    ));

    manager.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    assert!(
        wait_for(
            || sink.last_state() == Some(PairConnectionState::ConnectedPeerOffline),
            Duration::from_secs(15),
        )
        .await,
        "没有进入 ConnectedPeerOffline：errors={:?}",
        sink.errors()
    );

    // 用第二个连接观察心跳：客户端每心跳发一次 WS ping，中继应当回 pong
    let mut observer = client::connect(
        &relay,
        &room_id,
        &auth_token,
        e2e_server_token().as_deref(),
        "e2e-heartbeat-peer",
    )
    .await
    .unwrap();
    let _ = client::send_message(&mut observer, tungstenite_ping()).await;

    let heard_pong = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client::next_message(&mut observer).await {
                Some(Ok(Message::Pong(_))) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(heard_pong, "中继没有回 pong，心跳探活方案不成立");

    // 心跳间隔设为 2 秒（测试用），10 秒内不应触发重连
    tokio::time::sleep(Duration::from_secs(10)).await;

    let states: Vec<PairConnectionState> = sink
        .status_events(EVENT_CONNECTION_CHANGED)
        .iter()
        .filter_map(|payload| serde_json::from_value(payload.get("state")?.clone()).ok())
        .collect();

    assert!(
        !states.contains(&PairConnectionState::Reconnecting),
        "心跳期间发生了重连: {states:?}"
    );

    manager.disconnect();
}

fn tungstenite_ping() -> tokio_tungstenite::tungstenite::Message {
    Message::Ping(Vec::new().into())
}

/// 连不上中继时要进入重连状态，而不是卡在「正在连接」。
///
/// 这个用例故意指向一个必然拒绝连接的端口，所以不需要真的跑一个 relay。
#[tokio::test]
async fn unreachable_relay_enters_reconnecting() {
    let secret =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, [0u8; 32]);
    let sink = Arc::new(RecordingSink::default());
    let manager = Arc::new(PairManager::new(
        "e2e-reconnect".into(),
        sink.clone(),
        memory_history(),
        memory_store(),
    ));

    manager.start("http://127.0.0.1:1", Some(&secret), None).unwrap();

    let reconnecting = wait_for(
        || sink.last_state() == Some(PairConnectionState::Reconnecting),
        Duration::from_secs(30),
    )
    .await;

    assert!(
        reconnecting,
        "没有进入重连状态：state={:?} errors={:?}",
        sink.last_state(),
        sink.errors()
    );

    manager.disconnect();
}

/// `pair-connection-changed` 里最后一次的 P2P 状态
fn last_p2p(sink: &RecordingSink) -> Option<String> {
    let payload = sink.status_events(EVENT_CONNECTION_CHANGED).pop()?;

    payload.get("p2p")?.as_str().map(str::to_string)
}

/// 某一种连接状态广播出去时的 P2P 状态（取最后一次同状态的那条）
fn p2p_while(sink: &RecordingSink, state: &str) -> Option<String> {
    sink.status_events(EVENT_CONNECTION_CHANGED)
        .iter()
        .filter(|payload| payload["state"] == state)
        .filter_map(|payload| payload.get("p2p")?.as_str().map(str::to_string))
        .last()
}

/// 两个客户端通过真实中继交换信令并打通 P2P（Phase 8b）。
///
/// 两个 `PairManager` 各自连上真实中继，用 `pair.signal` 换 SDP 与 ICE candidate，
/// 然后靠 host candidate 开出一条 DataChannel。**DC 腿的探针是应用级 `pair.ping`，
/// pong 从同一条通道回来**：收不到 pong 的话两个心跳之后 `p2p` 会掉回 `connecting`，
/// 所以「连着几个心跳一直 `connected`」就等于 ping/pong 在 DC 上真的往返了。
///
/// 需要 `BONGO_PAIR_HEARTBEAT_SECS` 在 10 秒以内：默认 60 秒时这条用例要等三分钟。
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn two_clients_open_a_p2p_channel_through_the_relay() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let Some(heartbeat) = std::env::var(super::manager::HEARTBEAT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0 && *value <= 10)
    else {
        eprintln!("跳过：这条用例需要 BONGO_PAIR_HEARTBEAT_SECS <= 10");

        return;
    };

    let _guard = e2e_lock();

    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let manager_a = Arc::new(PairManager::new(
        "e2e-p2p-a".into(),
        sink_a.clone(),
        memory_history(),
        memory_store(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-p2p-b".into(),
        sink_b.clone(),
        memory_history(),
        memory_store(),
    ));

    manager_a.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&relay, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    // 打洞要等 ICE：本机两个进程之间是秒级，留 30 秒余量
    let opened = wait_for(
        || {
            last_p2p(&sink_a).as_deref() == Some("connected")
                && last_p2p(&sink_b).as_deref() == Some("connected")
        },
        Duration::from_secs(30),
    )
    .await;

    assert!(
        opened,
        "P2P 没有打通：A={:?} B={:?} errors={:?}",
        last_p2p(&sink_a),
        last_p2p(&sink_b),
        (sink_a.errors(), sink_b.errors())
    );

    // 三个心跳：DC 腿的探针超时窗口是两个心跳，跨过它还能保持 `connected` 就说明
    // pong 真的从 DC 回来了
    tokio::time::sleep(Duration::from_secs(heartbeat * 3)).await;

    assert_eq!(
        last_p2p(&sink_a).as_deref(),
        Some("connected"),
        "A 的 P2P 腿掉了：errors={:?}",
        sink_a.errors()
    );
    assert_eq!(
        last_p2p(&sink_b).as_deref(),
        Some("connected"),
        "B 的 P2P 腿掉了：errors={:?}",
        sink_b.errors()
    );

    // 中继那条腿不受影响：整条会话不该发生重连
    assert_eq!(sink_a.last_state(), Some(PairConnectionState::Connected));
    assert_eq!(sink_b.last_state(), Some(PairConnectionState::Connected));

    // R30：DC 验过之后可覆盖流走 DC。这里只做冒烟（A 发、B 收到、两边仍 `connected`）——
    // 「这一帧没经过中继」只能由单测的负向断言证明（真中继看到的是密文，分不出帧 kind，
    // 也没有计数器），见 `manager.rs` 的
    // `coverable_frames_take_the_data_channel_and_chat_never_does`。
    manager_a
        .send_replaceable(
            FrameKind::PetState,
            message_type::PET_STATE,
            serde_json::to_value(PetSnapshot::default()).unwrap(),
        )
        .unwrap();

    let snapshot = wait_for(
        || !sink_b.status_events(EVENT_PET_STATE).is_empty(),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        snapshot,
        "B 没有收到 A 的宠物快照：errors={:?}",
        sink_b.errors()
    );
    assert_eq!(last_p2p(&sink_a).as_deref(), Some("connected"));
    assert_eq!(last_p2p(&sink_b).as_deref(), Some("connected"));

    // 退避期间 `p2p` 必须已经复位（只读审计提出的 P1）：中继腿一掉，腿就被 Drop 了，
    // 而退避（最长 30 秒）+ 连接与 welcome 超时（15 + 10 秒）里不会再有 P2P 事件。
    // 触发方式是用**同 deviceId 的第三条连接**把 A 顶掉——中继会给旧连接发
    // 4002 `REPLACED`，这是本机能真实走到的掉线路径（4002 不是 fatal，所以进重连）。
    let secret = crypto::decode_pair_secret(&secret_text).unwrap();
    let auth_token = crypto::derive_auth_token(&secret);
    let room_id = crypto::derive_room_id(&secret);
    let replacement = client::connect(
        &relay,
        &room_id,
        &auth_token,
        e2e_server_token().as_deref(),
        "e2e-p2p-a",
    )
    .await
    .unwrap();

    let replaced = wait_for(
        || p2p_while(&sink_a, "reconnecting").is_some(),
        Duration::from_secs(15),
    )
    .await;

    assert!(
        replaced,
        "A 没有被顶替进入重连：{:?}",
        sink_a.status_events(EVENT_CONNECTION_CHANGED)
    );
    assert_eq!(
        p2p_while(&sink_a, "reconnecting").as_deref(),
        Some("off"),
        "退避期间的 `p2p` 没有复位"
    );

    // 放开那个占位连接，让 A 用同一个 deviceId 重新连上
    drop(replacement);

    manager_a.disconnect();
    manager_b.disconnect();

    // 断开之后这条腿的状态要复位：留着上一轮的 `connected` 会让偏好页说谎
    let reset = wait_for(
        || last_p2p(&sink_a).as_deref() == Some("off"),
        Duration::from_secs(5),
    )
    .await;

    assert!(reset, "断开后 P2P 状态没有复位：{:?}", last_p2p(&sink_a));
}

/// §10 的单机版「服务器转发量」观测：真的附件**走 DC**，中继在那个窗口里只看到信令。
///
/// 跨 NAT 的成功率只能人工双机验；但「分片到底走没走中继」在本机就能量出来：用例在中继
/// 前面挡一个纯 TCP 的字节计数器，两个客户端都连到它上面。作为对照，同一条链路上
/// `two_clients_exchange_a_file_through_the_relay` 会把整个文件都推过服务器——**它不等
/// P2P**（offer 早于 `reliable_verified`，这一单因此钉在中继上），自己并不校验路由；那份
/// 对照是单独量过的：同一个 1.5 MiB 的负载让中继经手 1,572,864 字节 + 约 5 KB 开销。
///
/// 需要 `http://host:port` 形式的 relay（计数代理只转发裸 TCP），本机自建中继就是这一种。
/// 别的形态（例如已经部署好的 https 中继）上这条用例会**跳过**——打印一行就返回，而
/// `--ignored` 全跑时跳过的用例仍算通过，所以「7/7」在这条用例上不等于「计数真的量过」。
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn a_file_takes_the_data_channel_and_barely_touches_the_relay() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let Some(upstream) = relay
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .parse::<SocketAddr>()
        .ok()
    else {
        eprintln!("跳过：这条用例需要 `http://host:port` 形式的 relay，收到 {relay}");

        return;
    };

    let _guard = e2e_lock();

    let (proxy, counts) = counting_proxy(upstream).await;
    let through_proxy = format!("http://{proxy}");

    let store_a = memory_store();
    let store_b = memory_store();
    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let history_a = memory_history();
    let history_b = memory_history();
    let manager_a = Arc::new(PairManager::new(
        "e2e-dc-file-a".into(),
        sink_a.clone(),
        Arc::clone(&history_a),
        store_a.clone(),
    ));
    let manager_b = Arc::new(PairManager::new(
        "e2e-dc-file-b".into(),
        sink_b.clone(),
        Arc::clone(&history_b),
        store_b.clone(),
    ));

    manager_a.start(&through_proxy, Some(&secret_text), e2e_server_password().as_deref()).unwrap();
    manager_b.start(&through_proxy, Some(&secret_text), e2e_server_password().as_deref()).unwrap();

    let connected = wait_for(
        || {
            sink_a.last_state() == Some(PairConnectionState::Connected)
                && sink_b.last_state() == Some(PairConnectionState::Connected)
        },
        Duration::from_secs(15),
    )
    .await;

    assert!(
        connected,
        "两端没有进入 Connected：A={:?} B={:?} errors={:?}",
        sink_a.last_state(),
        sink_b.last_state(),
        (sink_a.errors(), sink_b.errors())
    );

    // 打洞要等 ICE：本机两个进程之间是秒级，留 30 秒余量
    let opened = wait_for(
        || {
            last_p2p(&sink_a).as_deref() == Some("connected")
                && last_p2p(&sink_b).as_deref() == Some("connected")
        },
        Duration::from_secs(30),
    )
    .await;

    assert!(
        opened,
        "P2P 没有打通：A={:?} B={:?} errors={:?}",
        last_p2p(&sink_a),
        last_p2p(&sink_b),
        (sink_a.errors(), sink_b.errors())
    );

    // 附件那一单在 offer 之前就钉腿，而 `reliable` 那条通道要等它自己的 ping / pong 回来
    // 才算「验过」（`reliable_verified`；UI 的 `p2p` 只描述可覆盖那条腿）。本机往返是毫秒
    // 级，这里给 2 秒。真有偏差也不会假通过——下面那条断言会把绕中继的一单抓出来。
    tokio::time::sleep(Duration::from_secs(2)).await;

    let (to_relay_before, _) = counts.snapshot();

    assert!(
        to_relay_before > 0,
        "计数代理不在链路上：握手与打洞的信令也该经过它"
    );

    // A 发一个 1.5 MiB 的附件：多一个字节，让最后一块是真实的**短块**（Direct 那一单按
    // 48 KiB 切，32 块整块 + 1 个 1 字节的短块）
    let payload: Vec<u8> = (0..(1024 * 1536 + 1))
        .map(|index| (index % 251) as u8)
        .collect();
    let source = store_a.root().join("直连分片.bin");

    std::fs::create_dir_all(store_a.root()).unwrap();
    std::fs::write(&source, &payload).unwrap();

    let (sha256, size) = sha256_file(&source).unwrap();
    let message_id = "e2e-dc-file-message".to_string();
    let attachment_id = "e2e-dc-file-attachment".to_string();

    history_a
        .upsert_attachment(&NewAttachment {
            id: attachment_id.clone(),
            kind: MessageKind::File,
            original_name: Some("直连分片.bin".into()),
            mime: Some("application/octet-stream".into()),
            size: Some(size),
            sha256: Some(sha256.clone()),
            local_path: Some(source.to_string_lossy().to_string()),
            created_at: 0,
        })
        .unwrap();
    history_a
        .insert(&NewMessage::outgoing_attachment(
            message_id.clone(),
            MessageKind::File,
            attachment_id.clone(),
            0,
            history_a.epoch().unwrap(),
        ))
        .unwrap();

    manager_a
        .start_transfer(OutgoingRequest {
            transfer_id: super::manager::new_transfer_id(),
            message_id: message_id.clone(),
            attachment_id,
            kind: TransferKind::File,
            name: "直连分片.bin".into(),
            mime: "application/octet-stream".into(),
            size,
            sha256,
            path: source,
        })
        .unwrap();

    // B 收完并落盘
    let received = wait_for(
        || {
            history_b
                .find(&message_id)
                .ok()
                .flatten()
                .map(|message| message.status)
                == Some(MessageStatus::Received)
        },
        Duration::from_secs(30),
    )
    .await;

    assert!(
        received,
        "B 没有收下附件：errors={:?}",
        (sink_b.errors(), sink_a.errors())
    );

    let (to_relay_after, _) = counts.snapshot();
    let forwarded = to_relay_after - to_relay_before;

    let message = history_b.find(&message_id).unwrap().unwrap();
    let path = message
        .attachment
        .expect("B 的附件记录")
        .local_path
        .expect("B 的落盘路径");

    assert_eq!(std::fs::read(&path).unwrap(), payload, "落盘内容必须一致");

    // 这一条是「服务器转发量下降」的单机证据：文件走了 DC 的话，中继在这个窗口里只剩
    // 心跳（与可能的状态帧）。真走中继的话它会至少看到整个文件的大小。
    assert!(
        forwarded < payload.len() as u64 / 8,
        "这一单看起来绕了中继：窗口内中继经手 {forwarded} 字节（文件 {} 字节）",
        payload.len()
    );

    // 传完这条腿还得在：附件不该把 DC 用坏
    assert_eq!(
        last_p2p(&sink_a).as_deref(),
        Some("connected"),
        "A 的 P2P 腿在传输后掉了：errors={:?}",
        sink_a.errors()
    );
    assert_eq!(
        last_p2p(&sink_b).as_deref(),
        Some("connected"),
        "B 的 P2P 腿在传输后掉了：errors={:?}",
        sink_b.errors()
    );

    eprintln!(
        "单机观测：文件 {} 字节，窗口内中继经手 {forwarded} 字节（{}%）",
        payload.len(),
        forwarded * 100 / payload.len() as u64
    );

    manager_a.disconnect();
    manager_b.disconnect();
}

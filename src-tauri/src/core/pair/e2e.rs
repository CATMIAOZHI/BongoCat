//! 针对真实中继的端到端测试。
//!
//! 这些用例需要一个正在运行的 relay（本地 `pnpm dev` 或已部署的 Worker），
//! 所以默认被 `#[ignore]` 跳过。运行方式：
//!
//! ```powershell
//! $env:BONGO_PAIR_E2E_RELAY = "http://127.0.0.1:8787"
//! $env:BONGO_PAIR_E2E_SECRET = "<generate-pair.mjs 输出的 Pair Secret>"
//! $env:BONGO_PAIR_HEARTBEAT_SECS = "2"
//! cargo test --manifest-path src-tauri/Cargo.toml --lib pair::e2e -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::client;
use super::crypto::{self};
use super::history::{MessageStatus, PairHistory};
use super::history::{MessageKind, NewAttachment, NewMessage};
use super::manager::OutgoingRequest;
use super::transfer::{TransferStore, sha256_file};
use super::manager::{
    EVENT_CONNECTION_CHANGED, EVENT_MESSAGE_RECEIVED, EVENT_MESSAGE_UPDATED, EVENT_PEER_CHANGED,
    EVENT_PRESENCE, PairConnectionState, PairManager,
};
use super::protocol::{FrameKind, PresencePayload, PresenceState, TransferKind, message_type};

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

/// 所有端到端用例共用同一个 Durable Object（一个部署实例永远只有一对用户），
/// 所以必须串行执行：并行时第二个用例的 deviceId 会被当成第三人并以 4003 拒绝。
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

    manager_a.start(&relay, Some(&secret_text)).unwrap();
    manager_b.start(&relay, Some(&secret_text)).unwrap();

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

    manager_a.start(&relay, Some(&secret_text)).unwrap();
    manager_b.start(&relay, Some(&secret_text)).unwrap();

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

    manager_b.start(&relay, Some(&secret_text)).unwrap();

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

    manager_a.start(&relay, Some(&secret_text)).unwrap();
    manager_b.start(&relay, Some(&secret_text)).unwrap();

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
    manager_b.start(&relay, Some(&secret_text)).unwrap();

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

    manager_a.start(&relay, Some(&secret_text)).unwrap();
    manager_b.start(&relay, Some(&secret_text)).unwrap();

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

    let sink = Arc::new(RecordingSink::default());
    let manager = Arc::new(PairManager::new(
        "e2e-heartbeat".into(),
        sink.clone(),
        memory_history(),
        memory_store(),
    ));

    manager.start(&relay, Some(&secret_text)).unwrap();

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
    let mut observer = client::connect(&relay, &auth_token, "e2e-heartbeat-peer")
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

    manager.start("http://127.0.0.1:1", Some(&secret)).unwrap();

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

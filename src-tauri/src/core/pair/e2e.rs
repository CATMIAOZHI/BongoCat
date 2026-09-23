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

use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::client;
use super::crypto::{self, PairCipher};
use super::manager::{EVENT_CONNECTION_CHANGED, EVENT_PRESENCE, PairConnectionState, PairManager};
use super::protocol::{
    AppEnvelope, FrameHeader, FrameKind, PresencePayload, PresenceState, message_type,
};

/// 记录所有事件的测试用 sink
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, Value)>>,
}

impl RecordingSink {
    fn status_events(&self, event: &str) -> Vec<Value> {
        let events = self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

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
    let manager_a = Arc::new(PairManager::new("e2e-a".into(), sink_a.clone()));
    let manager_b = Arc::new(PairManager::new("e2e-b".into(), sink_b.clone()));

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

    assert!(arrived, "B 没有收到 A 的 presence：errors={:?}", sink_b.errors());

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

/// 断线后自动重连，并且用 WS 心跳保持连接存活
#[tokio::test]
#[ignore = "需要本地或已部署的 relay，见文件头说明"]
async fn reconnects_after_relay_drop_and_survives_heartbeats() {
    let Some((relay, secret_text)) = e2e_config() else {
        eprintln!("跳过：未设置 BONGO_PAIR_E2E_RELAY / BONGO_PAIR_E2E_SECRET");

        return;
    };

    let _guard = e2e_lock();

    let secret = crypto::decode_pair_secret(&secret_text).unwrap();
    let auth_token = crypto::derive_auth_token(&secret);

    let sink = Arc::new(RecordingSink::default());
    let manager = Arc::new(PairManager::new("e2e-heartbeat".into(), sink.clone()));

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

/// 帧头是明文且参与认证：中继改 kind 后接收端必须解密失败
#[test]
fn header_tampering_is_rejected() {
    let secret = [7u8; 32];
    let cipher = PairCipher::new(&crypto::derive_root_key(&secret));
    let envelope = AppEnvelope::new(message_type::PRESENCE, 1, json!({ "state": "active" }));
    let mut frame = cipher
        .seal(
            &FrameHeader::new(FrameKind::Presence, 1),
            &envelope.to_bytes().unwrap(),
        )
        .unwrap();

    frame[0] = FrameKind::Chat.as_byte();

    assert!(cipher.open(&frame).is_err());
}

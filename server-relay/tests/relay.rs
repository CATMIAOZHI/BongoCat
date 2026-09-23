//! 端到端：真的起一个中继、真的连 WebSocket，逐条验证线上契约。
//!
//! 这里覆盖的都是「客户端看到的行为」：健康检查与 404、鉴权 / 协议 / deviceId 的
//! HTTP 错误、`server.welcome` / `server.peer`、A↔B 转发、text 被拒、未知 kind、
//! 单帧过大、限流、配对已满、同 deviceId 顶替。这些行为必须与 `server-cloudflare/`
//! 一致，否则换 URL 就会有功能差异。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use bongocat_pair_relay::protocol::{self, close_code, Limits};
use bongocat_pair_relay::relay::Relay;
use bongocat_pair_relay::server::{self, Config};

const TOKEN: &str = "test-token";
const PATIENCE: Duration = Duration::from_secs(5);

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start_relay(limits: Limits, stale_after: Duration) -> SocketAddr {
    start_relay_with(limits, stale_after, None).await
}

async fn start_relay_with(
    limits: Limits,
    stale_after: Duration,
    ice_servers: Option<serde_json::Value>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        token: TOKEN.to_string(),
        limits,
        stale_after,
        ice_servers: ice_servers.clone(),
    });
    let relay = Relay::new(limits, stale_after, ice_servers);

    tokio::spawn(server::serve(listener, config, relay));

    address
}

async fn connect(
    address: SocketAddr,
    token: &str,
    protocol_version: &str,
    device_id: &str,
) -> Result<Client, WsError> {
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();

    {
        let headers = request.headers_mut();

        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("x-bongo-client", device_id.parse().unwrap());
        headers.insert("x-bongo-protocol", protocol_version.parse().unwrap());
    }

    let (socket, _) = connect_async(request).await?;

    Ok(socket)
}

/// 握手成功返回 101，否则返回 HTTP 状态码
async fn handshake_status(
    address: SocketAddr,
    token: &str,
    protocol_version: &str,
    device_id: &str,
) -> u16 {
    match connect(address, token, protocol_version, device_id).await {
        Ok(_) => 101,
        Err(WsError::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("期望 HTTP 错误，实际 {other:?}"),
    }
}

async fn raw_request(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();

    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();

    stream.read_to_string(&mut response).await.unwrap();
    response
}

async fn next_json(client: &mut Client) -> serde_json::Value {
    loop {
        let message = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等控制帧超时")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

async fn wait_close(client: &mut Client) -> Option<u16> {
    loop {
        let message = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等关闭帧超时")?;

        match message {
            Ok(Message::Close(Some(frame))) => return Some(frame.code.into()),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn frame(kind: u8, payload: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; protocol::FRAME_HEADER_SIZE];

    bytes[0] = kind;
    bytes.extend(std::iter::repeat_n(7u8, payload));
    bytes
}

#[tokio::test]
async fn health_is_public_and_unknown_paths_are_not_found() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let response = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 200 OK"), "实际：{response}");
    assert!(response.contains("\"ok\":true"));
    assert!(response.contains("\"protocol\":1"));

    let response = raw_request(address, "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 404 "), "实际：{response}");
}

#[tokio::test]
async fn rejects_bad_auth_protocol_and_device_ids() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    assert_eq!(handshake_status(address, "wrong", "1", "aaaa").await, 401);
    assert_eq!(handshake_status(address, TOKEN, "2", "aaaa").await, 426);
    assert_eq!(handshake_status(address, TOKEN, "1", "bad id").await, 400);
    assert_eq!(handshake_status(address, TOKEN, "1", "").await, 400);
    // 先鉴权再看 deviceId：错 token 配非法 deviceId 也必须是 401，
    // 否则未鉴权的人能靠响应码探测 deviceId 的合法性
    assert_eq!(handshake_status(address, "wrong", "1", "bad id").await, 401);
    assert_eq!(handshake_status(address, TOKEN, "1", "aaaa").await, 101);

    // 不是 WebSocket 升级
    let response = raw_request(address, "GET /ws HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 426 "), "实际：{response}");
}

#[tokio::test]
async fn rejects_a_broken_websocket_handshake() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    // 缺 Sec-WebSocket-Key
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nX-Bongo-Protocol: 1\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // Key 不是 16 字节的 base64
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: short\r\nX-Bongo-Protocol: 1\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // 版本不是 13：按 RFC 6455 §4.4 回 426 并带上支持的版本
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 8\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nX-Bongo-Protocol: 1\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 426 "), "实际：{response}");
    assert!(
        response.contains("Sec-WebSocket-Version: 13"),
        "实际：{response}"
    );
}

#[tokio::test]
async fn welcome_peer_state_and_forwarding_match_the_cloudflare_relay() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut first = connect(address, TOKEN, "1", "aaaa").await.unwrap();
    let welcome = next_json(&mut first).await;

    assert_eq!(welcome["type"], "server.welcome");
    assert_eq!(welcome["protocol"], 1);
    assert_eq!(welcome["peerOnline"], false);
    assert_eq!(welcome["limits"]["framesPerSecond"], 30.0);
    assert_eq!(welcome["limits"]["chunksPerSecond"], 20.0);
    assert_eq!(welcome["limits"]["bytesPerSecond"], 12.0 * 1024.0 * 1024.0);
    assert!(welcome.get("iceServers").is_none());

    let mut second = connect(address, TOKEN, "1", "bbbb").await.unwrap();

    assert_eq!(next_json(&mut second).await["peerOnline"], true);

    let announced = next_json(&mut first).await;

    assert_eq!(announced["type"], "server.peer");
    assert_eq!(announced["online"], true);
    assert_eq!(announced["deviceId"], "bbbb");

    // 应用帧只转发给对端
    // kind 4 = chat：随便挑一个已知 kind，中继不看内容
    let sent = frame(4, 16);

    first
        .send(Message::Binary(sent.clone().into()))
        .await
        .unwrap();

    let received = tokio::time::timeout(PATIENCE, second.next())
        .await
        .expect("等转发帧超时")
        .unwrap()
        .unwrap();

    match received {
        Message::Binary(bytes) => assert_eq!(&bytes[..], &sent[..]),
        other => panic!("期望二进制帧，实际 {other:?}"),
    }
}

#[tokio::test]
async fn a_disconnect_is_announced_to_the_other_side() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut first = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut first).await;

    let second = connect(address, TOKEN, "1", "bbbb").await.unwrap();

    next_json(&mut first).await;
    drop(second);

    let announced = next_json(&mut first).await;

    assert_eq!(announced["type"], "server.peer");
    assert_eq!(announced["online"], false);
    assert_eq!(announced["deviceId"], "bbbb");
}

#[tokio::test]
async fn the_third_device_is_rejected_and_a_reconnect_replaces() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut first = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut first).await;

    let mut second = connect(address, TOKEN, "1", "bbbb").await.unwrap();

    next_json(&mut second).await;
    next_json(&mut first).await;

    let mut third = connect(address, TOKEN, "1", "cccc").await.unwrap();

    assert_eq!(wait_close(&mut third).await, Some(close_code::PAIR_FULL));

    // 同一个 deviceId 用大写重连：顶替旧连接（4002），不是第三人（4003）
    let mut reconnected = connect(address, TOKEN, "1", "AAAA").await.unwrap();

    assert_eq!(wait_close(&mut first).await, Some(close_code::REPLACED));
    assert_eq!(next_json(&mut reconnected).await["peerOnline"], true);
    assert_eq!(next_json(&mut second).await["deviceId"], "aaaa");
}

#[tokio::test]
async fn text_frames_close_with_1008() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;
    client.send(Message::Text("hi".into())).await.unwrap();

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn unknown_frame_kinds_close_with_1008() {
    // 每个子用例单独起一个中继：配对位只有两个，复用同一个端口会让「第三人」的
    // 判定和上一条连接的清理时机互相干扰，用例就会抖
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;
    client
        .send(Message::Binary(frame(99, 4).into()))
        .await
        .unwrap();

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn malformed_frames_close_with_1008() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;
    client
        .send(Message::Binary(vec![1u8; 5].into()))
        .await
        .unwrap();

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn an_oversized_frame_closes_with_1009() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;

    let oversized = vec![1u8; protocol::MAX_BINARY_FRAME_SIZE + 1];

    let _ = client.send(Message::Binary(oversized.into())).await;

    assert_eq!(wait_close(&mut client).await, Some(close_code::TOO_LARGE));
}

#[tokio::test]
async fn a_frame_far_above_the_protocol_limit_still_closes_with_1009() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;

    // 4 MiB：远高于协议上限（1 MiB），但仍在中继的 WebSocket 层上限（8 MiB）以内。
    //
    // 这条用例守的是「8 MiB 那段余量」：贴着 1 MiB 设 `max_message_size` 时，tungstenite
    // 读完帧头就会报错、帧体还留在接收缓冲里，关连接会让 TCP 直接 RST，客户端拿到的是
    // 「连接被重置」而不是 1009。能干净地收到 1009，就说明帧是被读完再关的。
    let oversized = vec![1u8; protocol::MAX_BINARY_FRAME_SIZE * 4];

    let _ = client.send(Message::Binary(oversized.into())).await;

    assert_eq!(wait_close(&mut client).await, Some(close_code::TOO_LARGE));
}

#[tokio::test]
async fn a_stale_connection_is_replaced_and_the_survivor_sees_it_go_offline() {
    // stale_after = 0：任何连接都算陈旧，C 进来时顶替先连进来的那个
    let address = start_relay(Limits::default(), Duration::ZERO).await;
    let mut first = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut first).await;

    let mut second = connect(address, TOKEN, "1", "bbbb").await.unwrap();

    next_json(&mut second).await;
    next_json(&mut first).await;

    let mut third = connect(address, TOKEN, "1", "cccc").await.unwrap();

    assert_eq!(next_json(&mut third).await["peerOnline"], true);

    // 被顶替的那条收到 4004
    assert_eq!(wait_close(&mut first).await, Some(close_code::STALE));

    // 存活方先收到「aaaa 离线」，再收到「cccc 上线」。离线这条 Cloudflare 版也会发
    // （`announceOffline` 只过滤同一 deviceId 的伪通知），顺序放在前面保证终态是在线。
    let offline = next_json(&mut second).await;

    assert_eq!(offline["online"], false);
    assert_eq!(offline["deviceId"], "aaaa");

    let online = next_json(&mut second).await;

    assert_eq!(online["online"], true);
    assert_eq!(online["deviceId"], "cccc");

    // 而且这条离线帧只发给多出来的那一方：新连接不该收到「对方离线」（CF 会连新连接
    // 一起发，那边的新连接反而会显示「对方离线」）
    let extra = tokio::time::timeout(Duration::from_millis(300), third.next()).await;

    assert!(extra.is_err(), "新连接不该收到离线通知: {extra:?}");
}

#[tokio::test]
async fn the_rate_limit_closes_with_1008() {
    // 只给 2 帧/秒：第 3 帧必然超限
    let limits = Limits {
        frames_per_second: 2.0,
        chunks_per_second: 2.0,
        bytes_per_second: 1024.0 * 1024.0,
    };
    let address = start_relay(limits, Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;

    for _ in 0..3 {
        let _ = client.send(Message::Binary(frame(1, 8).into())).await;
    }

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn the_relay_answers_pings() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();

    next_json(&mut client).await;
    client.send(Message::Ping(Vec::new().into())).await.unwrap();

    loop {
        let message = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等 Pong 超时")
            .unwrap()
            .unwrap();

        if let Message::Pong(_) = message {
            return;
        }
    }
}

#[tokio::test]
async fn the_welcome_advertises_configured_ice_servers() {
    let ice_servers = serde_json::json!([
        { "urls": ["stun:cat.example.com:3478"] },
        { "urls": ["turn:cat.example.com:3478"], "username": "bongo", "credential": "..." }
    ]);
    let address = start_relay_with(
        Limits::default(),
        Duration::from_secs(120),
        Some(ice_servers.clone()),
    )
    .await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();
    let welcome = next_json(&mut client).await;

    assert_eq!(welcome["iceServers"], ice_servers);
}

#[tokio::test]
async fn the_welcome_omits_ice_servers_when_not_configured() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect(address, TOKEN, "1", "aaaa").await.unwrap();
    let welcome = next_json(&mut client).await;

    assert!(welcome.get("iceServers").is_none());
}

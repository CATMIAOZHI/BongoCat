//! 端到端：真的起一个中继、真的连 WebSocket，逐条验证线上契约。
//!
//! 这里覆盖的都是「客户端看到的行为」：健康检查与 404、鉴权 / 协议 / deviceId / 会话
//! 标识的 HTTP 错误、`server.welcome` / `server.peer`、Room 内 A↔B 转发、**跨 Room
//! 的负向断言**、容量 503、text 被拒、未知 kind、单帧过大、限流、配对已满、同 deviceId
//! 顶替。这些行为必须与 `server-cloudflare/` 一致（除了「一个部署只服务一对用户」这条
//! 被多会话取代），否则换 URL 就会有功能差异。

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use bongocat_pair_relay::protocol::Tier;
use bongocat_pair_relay::relay::{Relay, RelayOptions, ServerKey};
use bongocat_pair_relay::server::{self, Config};
use bongocat_pair_relay::{
    auth,
    protocol::{
        self, close_code, Limits, DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE,
        DEFAULT_PRE_HANDSHAKE_PER_IP, DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND,
    },
};

/// 两个互不相干的会话：多会话的隔离性全靠它们来验
const ROOM_A: &str = "room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ROOM_B: &str = "room-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ROOM_C: &str = "room-cccccccccccccccccccccccccccccccccccccc";
const TOKEN_A: &str = "token-a";
const TOKEN_B: &str = "token-b";
const TOKEN_C: &str = "token-c";

/// R36：这一版中继**必须**配服务器密码，测试用一个固定值（长度满足最小值要求）
const SERVER_PASSWORD: &str = "relay-tests-server-password";
/// 公益档的密码（只在显式开了公益档的中继上有效）
const PUBLIC_PASSWORD: &str = "relay-tests-public-password";
/// 多把钥匙：同一个档配第二把（「每把钥匙各给一个人」）
const SECOND_SERVER_PASSWORD: &str = "relay-tests-second-full-key-01";
const SECOND_PUBLIC_PASSWORD: &str = "relay-tests-second-public-key-1";

fn server_token() -> String {
    auth::derive_server_token(SERVER_PASSWORD)
}

fn public_token() -> String {
    auth::derive_server_token(PUBLIC_PASSWORD)
}

/// 造一个合法的（43 个 base64url 字符）、和别人都不一样的会话标识。
///
/// 「服务器密码被拒的尝试一个名额都不占」这条用例必须**每次换一个 Room**才有意义：
/// 都用同一个 Room 的话，被拒的请求即使错误地占下了名额，同一个 Room 的后续连接
/// 也会因为摘要一致而照样成功，用例照样绿。
fn room(seed: char) -> String {
    format!("room-{}", seed.to_string().repeat(38))
}

const PATIENCE: Duration = Duration::from_secs(5);
/// 负向断言等的时长：足够让一条真的会串房的帧走到对面
const NEGATIVE_PATIENCE: Duration = Duration::from_millis(400);

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start_relay(limits: Limits, stale_after: Duration) -> SocketAddr {
    start_relay_with(limits, 20, stale_after, None).await
}

async fn start_relay_with(
    limits: Limits,
    max_sessions: usize,
    stale_after: Duration,
    ice_servers: Option<serde_json::Value>,
) -> SocketAddr {
    start_relay_full(limits, max_sessions, stale_after, ice_servers, None).await
}

async fn start_relay_full(
    limits: Limits,
    max_sessions: usize,
    stale_after: Duration,
    ice_servers: Option<serde_json::Value>,
    stun_port: Option<u16>,
) -> SocketAddr {
    start_relay_with_config(config(
        limits,
        max_sessions,
        stale_after,
        ice_servers,
        stun_port,
    ))
    .await
}

/// 集成测试缺省那份配置：公益档关着，其余取自会话层的缺省值。
///
/// 只有「配置 → 会话层」这一处映射（`Config::relay_options`），用例不再自己拼参数。
fn config(
    limits: Limits,
    max_sessions: usize,
    stale_after: Duration,
    ice_servers: Option<serde_json::Value>,
    stun_port: Option<u16>,
) -> Config {
    let defaults = RelayOptions::default();

    Config {
        limits,
        max_sessions,
        stale_after,
        ice_servers,
        stun_port,
        server_keys: vec![ServerKey::new(Tier::Full, SERVER_PASSWORD)],
        public_limits: defaults.public_limits,
        public_burst_frames: defaults.public_burst_frames,
        public_burst_bytes: defaults.public_burst_bytes,
        public_key_budget: defaults.public_key_budget,
        handshake_failures_per_minute: defaults.handshake_failures_per_minute,
        max_public_sessions: defaults.max_public_sessions,
        max_sessions_per_key: defaults.max_sessions_per_key,
        max_public_per_ip: defaults.max_public_per_ip,
        public_window: defaults.public_window,
        full_window: defaults.full_window,
        full_key_budget: defaults.full_key_budget,
        turn_secret: None,
        turn_ttl: defaults.turn_ttl,
        trust_proxy: false,
    }
}

/// 打开公益档：`max_public_sessions` / `max_public_per_ip` 与 `window` 由用例给定
fn public_config(
    max_sessions: usize,
    max_public_sessions: usize,
    max_public_per_ip: usize,
    window: Option<Duration>,
) -> Config {
    let mut config = config(
        Limits::default(),
        max_sessions,
        Duration::from_secs(120),
        None,
        None,
    );

    config
        .server_keys
        .push(ServerKey::new(Tier::Public, PUBLIC_PASSWORD));
    config.max_public_sessions = max_public_sessions;
    config.max_public_per_ip = max_public_per_ip;
    config.public_window = window;

    config
}

async fn start_relay_with_config(config: Config) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stun_port = config.stun_port;
    let relay = Relay::new(config.relay_options(stun_port));

    tokio::spawn(server::serve(listener, relay));

    address
}

async fn connect(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    protocol_version: &str,
    device_id: &str,
) -> Result<Client, WsError> {
    connect_with_server(
        address,
        room_id,
        token,
        protocol_version,
        device_id,
        &server_token(),
    )
    .await
}

/// 与 [`connect`] 相同，但可以指定（或省掉）`X-Bongo-Server`：只有 R36 的几条负向
/// 用例需要它，其它用例一律走 [`connect`]——这样「默认情况下服务器密码一定是对的」。
async fn connect_with_server(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    protocol_version: &str,
    device_id: &str,
    server: &str,
) -> Result<Client, WsError> {
    connect_with_forwarded(
        address,
        room_id,
        token,
        protocol_version,
        device_id,
        server,
        &[],
    )
    .await
}

/// 与 [`connect_with_server`] 相同，但可以再带上 `X-Forwarded-For`：**每一项都写成一行**
/// （`append` 而不是 `insert`）。
///
/// 这个形状是必须的：给一个已经存在的头 append 值，Go 的 `net/http`（Caddy 就是它）会把它
/// 写成**另一行**，而不是拼进同一行。所以我们读这个头时必须看每一行、并取整体最后一项。
async fn connect_with_forwarded(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    protocol_version: &str,
    device_id: &str,
    server: &str,
    forwarded_for: &[&str],
) -> Result<Client, WsError> {
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();

    {
        let headers = request.headers_mut();

        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("x-bongo-client", device_id.parse().unwrap());
        headers.insert("x-bongo-protocol", protocol_version.parse().unwrap());
        headers.insert("x-bongo-room", room_id.parse().unwrap());
        // 新客户端**永远**带这个（值固定 1）：它自己也不知道用户填的是哪一类密码
        headers.insert("x-bongo-tier", "1".parse().unwrap());

        if !server.is_empty() {
            headers.insert("x-bongo-server", server.parse().unwrap());
        }

        for value in forwarded_for {
            headers.append("x-forwarded-for", value.parse().unwrap());
        }
    }

    let (socket, _) = connect_async(request).await?;

    Ok(socket)
}

/// A 房默认那条连接（大多数用例只关心一个会话）
async fn connect_a(address: SocketAddr, device_id: &str) -> Client {
    connect(address, ROOM_A, TOKEN_A, "1", device_id)
        .await
        .unwrap()
}

/// 握手成功返回 101，否则返回 HTTP 状态码
async fn handshake_status(address: SocketAddr, room_id: &str, token: &str, device_id: &str) -> u16 {
    handshake_status_with(address, room_id, token, "1", device_id).await
}

async fn handshake_status_with(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    protocol_version: &str,
    device_id: &str,
) -> u16 {
    handshake_status_with_server(
        address,
        room_id,
        token,
        protocol_version,
        device_id,
        &server_token(),
    )
    .await
}

async fn handshake_status_with_server(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    protocol_version: &str,
    device_id: &str,
    server: &str,
) -> u16 {
    match connect_with_server(address, room_id, token, protocol_version, device_id, server).await {
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

/// 负向断言：这段时间内一条消息都不该来
async fn expect_silence(client: &mut Client, what: &str) {
    let quiet = tokio::time::timeout(NEGATIVE_PATIENCE, client.next()).await;

    assert!(quiet.is_err(), "{what} 收到了不该收到的消息：{quiet:?}");
}

async fn wait_close_frame(client: &mut Client) -> Option<CloseFrame> {
    loop {
        let message = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等关闭帧超时")?;

        match message {
            Ok(Message::Close(Some(frame))) => return Some(frame),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

async fn wait_close(client: &mut Client) -> Option<u16> {
    wait_close_frame(client)
        .await
        .map(|frame| u16::from(frame.code))
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
    // §28：只说自己是多会话模式，不暴露任何会话列表
    assert!(response.contains("\"mode\":\"multi-pair\""));
    // R36：部署者一条 curl 就能确认自己装对了（密码是必填项）
    assert!(response.contains("\"passwordRequired\":true"));
    // 钥匙是把数公开的：部署者配多把时靠它确认自己没写漏（不含任何能拿去试的东西）
    assert!(
        response.contains("\"serverKeys\":{\"full\":1,\"public\":0}"),
        "实际：{response}"
    );
    assert!(!response.contains(ROOM_A));

    let response = raw_request(address, "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 404 "), "实际：{response}");
}

/// R36：服务器密码是**最外层**的门槛。
///
/// 没有它的人不该能建会话、不该能探测 Room 是否存在、更不该走到 `server.welcome`
/// （那里带着按流量计费的 TURN 凭据）。两种拒绝（没带 / 带错）用同一个状态码 403，
/// 但响应体不同，便于部署者自查。
#[tokio::test]
async fn the_server_password_gates_the_upgrade() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    // 完全没带
    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", "").await,
        403
    );
    // 带错的
    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", "wrong-password").await,
        403
    );
    // 带对的：照常进
    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", &server_token()).await,
        101
    );

    // 响应体要把两种拒绝分开（客户端与部署者都靠它定位）
    let missing = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         X-Bongo-Protocol: 1\r\nX-Bongo-Room: room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\
         Authorization: Bearer token-a\r\n\r\n",
    )
    .await;

    assert!(missing.starts_with("HTTP/1.1 403 "), "实际：{missing}");
    assert!(
        missing.contains("server password required"),
        "实际：{missing}"
    );

    let wrong = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         X-Bongo-Protocol: 1\r\nX-Bongo-Room: room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\
         X-Bongo-Server: wrong-password\r\nAuthorization: Bearer token-a\r\n\r\n",
    )
    .await;

    assert!(wrong.starts_with("HTTP/1.1 403 "), "实际：{wrong}");
    assert!(wrong.contains("server password incorrect"), "实际：{wrong}");
}

/// R36：被 403 挡掉的尝试**一个名额都不该占**——否则陌生人用错的密码刷几下
/// 就能让别人进不来（那正好是这道门槛要防的事）。
#[tokio::test]
async fn a_rejected_server_password_never_consumes_a_session_slot() {
    let address = start_relay_with(Limits::default(), 1, Duration::from_secs(120), None).await;

    // 三次错密码各用一个**全新**的 Room：如果闸门被挪到占名额之后，它们会各建一个
    // Room 并把唯一的名额用光，下面那次真用户的连接就会拿到 503 而不是 101
    for seed in ['1', '2', '3'] {
        assert_eq!(
            handshake_status_with_server(address, &room(seed), TOKEN_C, "1", "aaaa", "wrong").await,
            403
        );
    }

    // 唯一的名额仍然留给真正的用户
    assert_eq!(
        handshake_status_with_server(address, &room('9'), TOKEN_C, "1", "aaaa", &server_token())
            .await,
        101
    );
}

#[tokio::test]
async fn rejects_bad_auth_protocol_and_device_ids() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    // 空的 Authorization 连摘要都算不出来
    assert_eq!(handshake_status(address, ROOM_A, "", "aaaa").await, 401);
    assert_eq!(
        handshake_status_with(address, ROOM_A, TOKEN_A, "2", "aaaa").await,
        426
    );
    assert_eq!(
        handshake_status(address, ROOM_A, TOKEN_A, "bad id").await,
        400
    );
    assert_eq!(handshake_status(address, ROOM_A, TOKEN_A, "").await, 400);
    assert_eq!(
        handshake_status(address, ROOM_A, TOKEN_A, "aaaa").await,
        101
    );

    // 已有会话上，错 token 配非法 deviceId 必须是 401：没通过房间校验的人
    // 不能靠状态码探测 deviceId 的合法性
    let first = connect_a(address, "aaaa").await;

    assert_eq!(
        handshake_status(address, ROOM_A, "wrong", "bad id").await,
        401
    );
    assert_eq!(
        handshake_status(address, ROOM_A, "wrong", "aaaa").await,
        401
    );

    drop(first);

    // 不是 WebSocket 升级
    let response = raw_request(address, "GET /ws HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 426 "), "实际：{response}");
}

#[tokio::test]
async fn the_room_header_is_required_and_validated() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    // R36：服务器密码在最前面，所以这条用例必须带上它，才能走到 Room 的校验
    let head = format!(
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         X-Bongo-Protocol: 1\r\nX-Bongo-Server: {}\r\nAuthorization: Bearer token-a\r\n\
         X-Bongo-Client: aaaa\r\n",
        server_token()
    );

    // 完全没带 X-Bongo-Room
    let response = raw_request(address, &format!("{head}\r\n")).await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // 字符集非法
    let response = raw_request(address, &format!("{head}X-Bongo-Room: bad room!\r\n\r\n")).await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // 太长（上界 64）
    let response = raw_request(
        address,
        &format!("{head}X-Bongo-Room: {}\r\n\r\n", "a".repeat(65)),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");
}

#[tokio::test]
async fn rejects_a_broken_websocket_handshake() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    // 缺 Sec-WebSocket-Key
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nX-Bongo-Protocol: 1\r\nX-Bongo-Room: room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // Key 不是 16 字节的 base64
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: short\r\nX-Bongo-Protocol: 1\r\nX-Bongo-Room: room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 400 "), "实际：{response}");

    // 版本不是 13：按 RFC 6455 §4.4 回 426 并带上支持的版本
    let response = raw_request(
        address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 8\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nX-Bongo-Protocol: 1\r\nX-Bongo-Room: room-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n",
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
    let mut first = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut first).await;

    assert_eq!(welcome["type"], "server.welcome");
    assert_eq!(welcome["protocol"], 1);
    assert_eq!(welcome["peerOnline"], false);
    assert_eq!(welcome["limits"]["framesPerSecond"], 30.0);
    assert_eq!(welcome["limits"]["chunksPerSecond"], 20.0);
    assert_eq!(welcome["limits"]["bytesPerSecond"], 12.0 * 1024.0 * 1024.0);
    assert!(welcome.get("iceServers").is_none());

    let mut second = connect_a(address, "bbbb").await;

    assert_eq!(next_json(&mut second).await["peerOnline"], true);

    let announced = next_json(&mut first).await;

    assert_eq!(announced["type"], "server.peer");
    assert_eq!(announced["online"], true);
    assert_eq!(announced["deviceId"], "bbbb");

    // 应用帧只转发给同会话的对端
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
    let mut first = connect_a(address, "aaaa").await;

    next_json(&mut first).await;

    let second = connect_a(address, "bbbb").await;

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
    let mut first = connect_a(address, "aaaa").await;

    next_json(&mut first).await;

    let mut second = connect_a(address, "bbbb").await;

    next_json(&mut second).await;
    next_json(&mut first).await;

    let mut third = connect_a(address, "cccc").await;

    // 关闭码是契约、reason 不是，但两边（本中继与 CF 版）说同一句话能省掉一次
    // 「为什么这边说的是 room」的排查，所以这里连 reason 一起钉住
    let full = wait_close_frame(&mut third)
        .await
        .expect("第三人没有被关掉");

    assert_eq!(u16::from(full.code), close_code::PAIR_FULL);
    let reason: &str = full.reason.as_ref();

    assert_eq!(reason, "pair is full");

    // 同一个 deviceId 用大写重连：顶替旧连接（4002），不是第三人（4003）
    let mut reconnected = connect_a(address, "AAAA").await;

    assert_eq!(wait_close(&mut first).await, Some(close_code::REPLACED));
    assert_eq!(next_json(&mut reconnected).await["peerOnline"], true);
    assert_eq!(next_json(&mut second).await["deviceId"], "aaaa");
}

/// §32 的「两台 A + 两台 B」在真实 socket 上的版本：帧与公告都不能串房
#[tokio::test]
async fn rooms_are_isolated_end_to_end() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;

    let mut a1 = connect(address, ROOM_A, TOKEN_A, "1", "a1").await.unwrap();

    next_json(&mut a1).await;

    let mut a2 = connect(address, ROOM_A, TOKEN_A, "1", "a2").await.unwrap();

    next_json(&mut a2).await;
    next_json(&mut a1).await;

    let mut b1 = connect(address, ROOM_B, TOKEN_B, "1", "b1").await.unwrap();

    next_json(&mut b1).await;

    let mut b2 = connect(address, ROOM_B, TOKEN_B, "1", "b2").await.unwrap();

    next_json(&mut b2).await;
    next_json(&mut b1).await;

    // A 房里的帧只到 A 房的另一个人
    let sent = frame(4, 24);

    a1.send(Message::Binary(sent.clone().into())).await.unwrap();

    let received = tokio::time::timeout(PATIENCE, a2.next())
        .await
        .expect("A 房内部应当收到转发帧")
        .unwrap()
        .unwrap();

    match received {
        Message::Binary(bytes) => assert_eq!(&bytes[..], &sent[..]),
        other => panic!("期望二进制帧，实际 {other:?}"),
    }

    // 负向断言：B 房两边都得安静
    expect_silence(&mut b1, "B 房的第一台").await;
    expect_silence(&mut b2, "B 房的第二台").await;

    // B 房下线也不该惊动 A 房
    drop(b2);

    expect_silence(&mut a1, "A 房（B 房下线时）").await;
    expect_silence(&mut a2, "A 房（B 房下线时）").await;

    // 而 B 房自己的两台之间照常工作（b1 只收到 b2 的离线公告，那正是该收到的）
    let offline = next_json(&mut b1).await;

    assert_eq!(offline["type"], "server.peer");
    assert_eq!(offline["online"], false);
    assert_eq!(offline["deviceId"], "b2");
}

/// §9：同一个会话上拿错配对密码 = 401，而且不会因此多出一个会话
#[tokio::test]
async fn a_wrong_token_on_an_existing_room_is_refused() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut first = connect_a(address, "aaaa").await;

    next_json(&mut first).await;

    assert_eq!(
        handshake_status(address, ROOM_A, "wrong", "bbbb").await,
        401
    );

    // 正确的那份密钥照旧能用
    let mut second = connect_a(address, "bbbb").await;

    assert_eq!(next_json(&mut second).await["peerOnline"], true);
}

/// §8 / §27：满员只挡**新建**会话，503 而不是关闭码；已有会话的第二个人照进不误
#[tokio::test]
async fn a_full_server_only_refuses_new_rooms() {
    let address = start_relay_with(Limits::default(), 1, Duration::from_secs(120), None).await;
    let mut first = connect_a(address, "aaaa").await;

    next_json(&mut first).await;

    // 第二个会话：服务器只有一个名额，已经用在 A 房上了
    assert_eq!(
        handshake_status(address, ROOM_B, TOKEN_B, "bbbb").await,
        503
    );

    // 已有会话的第二个人不受影响
    let mut second = connect_a(address, "bbbb").await;

    assert_eq!(next_json(&mut second).await["peerOnline"], true);

    // 最后一个客户端走了，名额让出来（§13 / §30）
    drop(second);
    drop(first);

    let deadline = tokio::time::Instant::now() + PATIENCE;

    while tokio::time::Instant::now() < deadline {
        if handshake_status(address, ROOM_C, TOKEN_C, "cccc").await == 101 {
            return;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("会话空了之后名额应当释放，但新会话始终被拒");
}

#[tokio::test]
async fn text_frames_close_with_1008() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect_a(address, "aaaa").await;

    next_json(&mut client).await;
    client.send(Message::Text("hi".into())).await.unwrap();

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn unknown_frame_kinds_close_with_1008() {
    // 每个子用例单独起一个中继：会话里的位置只有两个，复用同一个端口会让「第三人」的
    // 判定和上一条连接的清理时机互相干扰，用例就会抖
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect_a(address, "aaaa").await;

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
    let mut client = connect_a(address, "aaaa").await;

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
    let mut client = connect_a(address, "aaaa").await;

    next_json(&mut client).await;

    let oversized = vec![1u8; protocol::MAX_BINARY_FRAME_SIZE + 1];

    let _ = client.send(Message::Binary(oversized.into())).await;

    assert_eq!(wait_close(&mut client).await, Some(close_code::TOO_LARGE));
}

#[tokio::test]
async fn a_frame_far_above_the_protocol_limit_still_closes_with_1009() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect_a(address, "aaaa").await;

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
    let mut first = connect_a(address, "aaaa").await;

    next_json(&mut first).await;

    let mut second = connect_a(address, "bbbb").await;

    next_json(&mut second).await;
    next_json(&mut first).await;

    let mut third = connect_a(address, "cccc").await;

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
    expect_silence(&mut third, "新连接").await;
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
    let mut client = connect_a(address, "aaaa").await;

    next_json(&mut client).await;

    for _ in 0..3 {
        let _ = client.send(Message::Binary(frame(1, 8).into())).await;
    }

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

/// 公益档的额度是**它自己那一份**（`PAIR_PUBLIC_MAX_FRAMES_PER_SECOND` /
/// `PAIR_PUBLIC_MAX_BYTES_PER_SECOND`），和部署者那一档分开配。
///
/// 桶确实按档位建（`relay.rs` 里的 `Bucket::new(self.connection_quota(tier), now)`），但额度
/// 本身只在 welcome 的广告值和 `limits_for` 上被断言过——把桶写成部署者那一档的额度，
/// 别的用例一条都不会红。这一条真拿公益连接把额度打穿。
#[tokio::test]
async fn the_public_tier_has_its_own_rate_limit() {
    // 公益档只给 2 帧/秒、突发也只有 2 帧：第 3 帧必然超限（部署者那一档仍是
    // `Limits::default()`，远大于此）。突发必须跟着调小——它的缺省是 24 帧，只调速率的话
    // 前 24 帧都在突发额度里，这一条会误判成「限流没生效」。
    let config = Config {
        public_limits: Limits {
            frames_per_second: 2.0,
            chunks_per_second: 2.0,
            bytes_per_second: 1024.0 * 1024.0,
        },
        public_burst_frames: 2.0,
        ..public_config(20, 10, 4, None)
    };
    let address = start_relay_with_config(config).await;
    let mut client = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;

    next_json(&mut client).await;

    // 必须是信令（kind 8）：别的 kind 会先被白名单用 1008 关掉，测不到限流这一层
    for _ in 0..3 {
        client
            .send(Message::Binary(
                frame(protocol::FRAME_KIND_SIGNAL, 16).into(),
            ))
            .await
            .unwrap();
    }

    assert_eq!(
        wait_close(&mut client).await,
        Some(close_code::PROTOCOL_ERROR)
    );
}

/// 发一条消息，然后用一次 Ping/Pong 确认服务器**真的处理过它了**。
///
/// 同一个 TCP 流是按顺序读的，而 Pong 由 tungstenite 在读循环里自动回（不碰限流那一层），
/// 所以「收到 Pong」= 「前面那条消息已经走完处理」。跨连接做限流断言时必须要有这个同步点，
/// 否则「谁先把预算用光」会变成抽签。
async fn send_and_settle(client: &mut Client, message: Message) {
    client.send(message).await.unwrap();
    client.send(Message::Ping(Vec::new().into())).await.unwrap();

    loop {
        let next = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等 Pong 超时")
            .unwrap()
            .unwrap();

        if let Message::Pong(_) = next {
            return;
        }
    }
}

/// 公益档的滚动预算记在**钥匙**上（`PAIR_PUBLIC_KEY_BUDGET_BYTES`）：拿同一把钥匙的
/// 会话共用一个桶，另一把钥匙不受影响。
///
/// 与上一条的区别：那一条是「这条连接自己发太快」（关 1008），这一条是「同一把钥匙的
/// 两个**不同会话**合起来把它用光了」。它必须在真连接上验——`Config::relay_options`
/// 有没有把这一项折进会话层、另一把钥匙是不是真的不受影响，只有跑到这一步才看得出来。
///
/// 这一条的额度取得极小（2000 字节 ⇒ 回填只有约 0.55 B/s）：用完之后的等待要十几分钟，
/// 超过 `KEY_BUDGET_WAIT_LIMIT`，也就是**等不起**——所以它仍然关 4006。额度用完只是
/// **限速**那条路由下一条用例钉住（它把额度配成「正好一帧」，等待因此只有一秒多）。
#[tokio::test]
async fn the_public_key_budget_is_shared_by_two_sessions_of_one_key() {
    let mut config = public_config(20, 10, 4, None);

    // 第二把公益钥匙：验「换一把钥匙就是另一份预算」
    config
        .server_keys
        .push(ServerKey::new(Tier::Public, SECOND_PUBLIC_PASSWORD));
    // 预算小到一眼能数清：两帧 900 字节的载荷就用光（回填速率是它的 1/3600，测试里等于
    // 没有）。载荷取 900 而不是 16：帧头只有 14 字节，用 16 字节的帧要发几百条才够得着
    // 这层预算，而那条连接自己的突发（64 KiB）反而先成为限制。
    config.public_key_budget = Some(2_000.0);

    let address = start_relay_with_config(config).await;

    // 两个**不同**的会话（`ROOM_A` / `ROOM_B`），都用第一把公益钥匙
    let mut first = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;
    let mut second = connect_public(address, ROOM_B, TOKEN_B, "bbbb").await;

    next_json(&mut first).await;
    next_json(&mut second).await;

    let signal = |payload| Message::Binary(frame(protocol::FRAME_KIND_SIGNAL, payload).into());

    // 一人一帧：这把钥匙的预算还剩 172 字节
    send_and_settle(&mut first, signal(900)).await;
    send_and_settle(&mut second, signal(900)).await;

    // 第 3 帧无论从哪一边发都会越界——这里从**第二个**会话发，证明它借的是同一把钥匙的
    // 那份预算，而不是自己那条连接的那一份（那条还有 64 KiB 的突发、24 帧的额度）
    second.send(signal(900)).await.unwrap();

    let closed = wait_close_frame(&mut second).await.expect("应当被关掉");

    assert_eq!(u16::from(closed.code), close_code::KEY_BUDGET);
    assert!(
        closed.reason.contains("budget"),
        "关闭帧要说清是**预算**用完了（不是「你发太快」），实际：{:?}",
        closed.reason
    );

    // 换一把公益钥匙就是另一份预算：它不该被前一把的欠账连坐——这两帧合计 1828 字节，
    // 装得进它自己那份 2000。要是预算记成了「档位」或者别的共用的东西，它们会被当场拒掉
    let mut other = connect_with_server(
        address,
        ROOM_C,
        TOKEN_C,
        "1",
        "cccc",
        &auth::derive_server_token(SECOND_PUBLIC_PASSWORD),
    )
    .await
    .unwrap();

    next_json(&mut other).await;

    for _ in 0..2 {
        other.send(signal(900)).await.unwrap();
    }

    expect_silence(&mut other, "拿另一把公益钥匙的连接").await;
}

/// 钥匙那份额度用完**不当场断**：等一小会儿接着传（这就是「用完限速」）。
///
/// 这一条必须在真连接上验：等待发生在 `Relay::serve` 的读循环里（等待期间**不读**这条
/// 连接，TCP 背压就是限速本身），而单测只到 `Relay::allow` 那一层，看不到它接没接好。
///
/// 数字全是配出来的，没有为测试改生产代码：把额度设成**正好一帧 24 KiB**，那一发就把桶
/// 花光；接着那个 14 字节的小帧这时要等 `3600 × 14 / 24590` ≈ 2 秒才装得下，仍在 5 秒
/// 的等待上限之内。老行为会当场用 4006 关掉这条连接（对端也就永远收不到那一帧），新行为
/// 是睡一下再把它转过去——所以这里既断言「转到了」，也断言「真的等了」。
#[tokio::test]
async fn a_spent_key_budget_slows_a_public_connection_instead_of_cutting_it() {
    let big = frame(protocol::FRAME_KIND_SIGNAL, 24 * 1024);
    let mut config = public_config(20, 10, 4, None);

    // 额度正好等于这一帧：发完它，这把钥匙的桶就空了
    config.public_key_budget = Some(big.len() as f64);

    let address = start_relay_with_config(config).await;

    // 同一个 Room 的两条公益连接：一条发、一条收（要看到「真的转过去了」）
    let mut sender = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;
    let mut peer = connect_public(address, ROOM_A, TOKEN_A, "bbbb").await;

    next_json(&mut sender).await;
    next_json(&mut peer).await;
    // 后进来的那条上线时，先来的会收到一条公告
    next_json(&mut sender).await;

    send_and_settle(&mut sender, Message::Binary(big.clone().into())).await;

    assert_eq!(next_binary(&mut peer).await, big, "第一帧该照常转发");

    // 额度已经用完：这一帧不是被拒，而是等一小会儿之后被转发出去
    let small = frame(protocol::FRAME_KIND_SIGNAL, 0);
    let started = std::time::Instant::now();

    sender
        .send(Message::Binary(small.clone().into()))
        .await
        .unwrap();

    assert_eq!(
        next_binary(&mut peer).await,
        small,
        "额度用完只是限速：这一帧该等到装得下之后转过去，而不是把连接关掉"
    );
    // 而且它是**等到**才过去的：额度只剩 0、小帧 14 字节，按每秒 6.8 字节回填 ≈ 2 秒。
    // 没有「等」这一环（比如把 `Allowance::Wait` 当成 `Pass`）的话这里立刻就到，断言会红
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "额度用完该被压到回填速度上（这一帧要等约 2 秒），实际只用了 {:?}",
        started.elapsed()
    );

    // 连接还开着（真被 4006 关掉的话，这里立刻会收到那条关闭帧，而不是安静超时）
    expect_silence(&mut sender, "额度用完之后的发送方").await;
}

/// **未鉴权的半开连接一条真实额度都不占**（审计里的 P0-1）。
///
/// 修之前的形状是「先领真实额度的许可，再读请求头」：随便谁开几条**不发任何数据**的 TCP
/// （每条只要在握手超时之内重连一次），就能把整池占住——`/health` 与所有正常握手一起拿
/// 503，而攻击者什么都不用做。现在读请求头那一段走的是另一道**宽得多**的闸（它只给
/// 「任务 + 8 KiB 请求头缓冲」一个硬上界），真实额度只在「鉴权与会话判定都过了」之后才领，
/// 所以这些半开的连接一个真实额度都不占。
///
/// 这里连的条数（34）正是 `max_sessions = 1` 时的真实额度：修之前它们刚好把闸占满。
#[tokio::test]
async fn idle_connections_never_consume_the_connection_budget() {
    // 1 组完全档 + 0 组公益档 = 2 × 1 + 32 = 34 条真实额度
    let config = Config {
        max_public_sessions: 0,
        ..config(Limits::default(), 1, Duration::from_secs(120), None, None)
    };
    let address = start_relay_with_config(config).await;

    // 只连不发：它们会一直挂在「预握手」那一段，而不是真实额度那一段
    let idle: Vec<TcpStream> = {
        let mut idle = Vec::new();

        for _ in 0..34 {
            idle.push(TcpStream::connect(address).await.unwrap());
        }

        idle
    };

    // 健康检查照旧：它排在限额之前（这一条也钉住「健康检查不会被别人的半开连接影响」）
    let health = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(
        health.contains(" 200 "),
        "半开连接不该挡住 /health，实际：{health}"
    );

    // 真实握手也照常：这些半开连接一个真实额度都没占
    assert_eq!(
        handshake_status(address, ROOM_A, TOKEN_A, "aaaa").await,
        101,
        "半开连接不该挡住正常握手"
    );

    drop(idle);
}

/// 预握手那道闸满了之后**当场回 503**，而不是把半开连接排进队列里等。
///
/// 撞上的是每来源那一层（`DEFAULT_PRE_HANDSHAKE_PER_IP`，默认 64）：同一个地址把额度用光
/// 之后，下一条**连请求头都没读**就被挡回去。这份配置下真实额度是 `2 × 1 + 32 = 34`，
/// 比 64 小，所以先撞上的一定是每来源那个数——用例因此同时钉住了「哪一道闸在先」与「那条
/// 拒绝路上真的把 503 写出去了」（不 drain 的话客户端只会看到 RST）。
///
/// 请求用 `/health` 是有意的：它排在一切限额之前，所以「拿到 200」就等于「预握手那道闸
/// 没被这道请求撞上」，对照明确。
#[tokio::test]
async fn the_pre_handshake_gate_answers_with_its_own_503() {
    let config = Config {
        max_public_sessions: 0,
        ..config(Limits::default(), 1, Duration::from_secs(120), None, None)
    };
    let address = start_relay_with_config(config).await;

    // 只连不发：这些会一直挂在「预握手」那一段
    let idle: Vec<TcpStream> = {
        let mut idle = Vec::new();

        for _ in 0..DEFAULT_PRE_HANDSHAKE_PER_IP {
            idle.push(TcpStream::connect(address).await.unwrap());
        }

        idle
    };

    // 上面那些许可是各自被 `accept` 之后才领的，所以这里要等它们都登记上
    let request = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut refused = String::new();

    for _ in 0..40 {
        refused = raw_request(address, request).await;

        if refused.contains(" 503 ") {
            break;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        refused.contains(" 503 ") && refused.contains("server is at its connection limit"),
        "预握手额度用光之后该回 503，实际：{refused}"
    );

    drop(idle);
}

/// 拿错服务器密码刷 `/ws`：额度（默认 30/分钟）用光之后，这个 IP 会**在鉴权之前**被 429
/// 挡下。
///
/// 这一条钉的是「闸放在哪一步」。放在鉴权之后的话，每一次被拒仍然要跑一遍摘要比较——
/// 而这道闸要省的正是那件事（外加日志）。
///
/// **`/health` 不在这道闸后面**：它排在限额之前。被 429 的地址仍然该能拿到健康检查，否则
/// 「有人在刷错密码」会同时让监控与被封互相掩盖——而容器健康检查走的正是那条路，健康检查
/// 失败会让 docker 判 `unhealthy` 并重启容器。
#[tokio::test]
async fn a_flood_of_failed_handshakes_is_429_before_authentication() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let stranger = auth::derive_server_token("relay-tests-stranger-password");

    for index in 1..=(DEFAULT_HANDSHAKE_FAILURES_PER_MINUTE as usize) {
        assert_eq!(
            handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", &stranger).await,
            403,
            "第 {index} 次仍按「密码不对」拒"
        );
    }

    // 第 31 次：这一次拿的是**对**的密码，照样被挡——闸在鉴权之前
    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", &server_token()).await,
        429
    );

    // 健康检查排在这道闸之前：被 429 也一样拿得到
    let health = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(health.contains(" 200 "), "实际：{health}");
    assert!(health.contains("\"ok\":true"), "实际：{health}");
}

#[tokio::test]
async fn the_relay_answers_pings() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect_a(address, "aaaa").await;

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
        20,
        Duration::from_secs(120),
        Some(ice_servers.clone()),
    )
    .await;
    let mut client = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut client).await;

    assert_eq!(welcome["iceServers"], ice_servers);
}

/// coturn REST API 的那份凭据：`base64(HMAC-SHA1(共享密钥, username))`。
///
/// 用例自己算一遍（不复用被测代码）。算法本身由 `relay.rs` 里钉 RFC 2202 向量的那条用例
/// 保证，所以这里验的是**接线**：配置有没有真的折进会话层、welcome 里那份凭据是不是现签的。
fn expected_turn_credential(secret: &str, username: &str) -> String {
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};

    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes()).unwrap();

    mac.update(username.as_bytes());

    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// 配了 `PAIR_TURN_SECRET` 之后，welcome 里的 `turn:` 凭据由中继**现签**（限时），而不是
/// `PAIR_ICE_SERVERS` 里那份长期有效的静态值。
///
/// 走的是完整链路（`Config` → `RelayOptions` → welcome），所以顺带钉住「配置项真的折进
/// 会话层了」——漏一处只表现为「凭据还是旧的」，而那与「TURN 被别人白用」是同一件事。
#[tokio::test]
async fn a_configured_turn_secret_signs_short_lived_credentials() {
    const SECRET: &str = "relay-tests-turn-shared-secret";

    let ice_servers = serde_json::json!([
        { "urls": ["stun:cat.example.com:3478"] },
        {
            "urls": ["turn:cat.example.com:3478"],
            "username": "static-user",
            "credential": "static-pass"
        }
    ]);
    let config = Config {
        turn_secret: Some(SECRET.to_string()),
        turn_ttl: Duration::from_secs(600),
        ..config(
            Limits::default(),
            20,
            Duration::from_secs(120),
            Some(ice_servers),
            None,
        )
    };
    let address = start_relay_with_config(config).await;
    let mut client = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut client).await;
    let advertised = &welcome["iceServers"];

    // `stun:` 那条原样留着
    assert_eq!(
        advertised[0],
        serde_json::json!({ "urls": ["stun:cat.example.com:3478"] })
    );
    // `turn:` 那条换成了限时凭据
    assert_ne!(advertised[1]["credential"], "static-pass");

    let username = advertised[1]["username"].as_str().unwrap();
    let (expire, identity) = username.split_once(':').expect("形状要是 {过期}:{标识}");
    let expire: u64 = expire.parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    assert!(
        expire > now + 500 && expire <= now + 600,
        "过期时刻要落在 TTL 里：{expire}（现在 {now}）"
    );
    assert_eq!(identity, "1");
    assert_eq!(
        advertised[1]["credential"].as_str().unwrap(),
        expected_turn_credential(SECRET, username)
    );
}

#[tokio::test]
async fn the_welcome_omits_ice_servers_when_not_configured() {
    let address = start_relay(Limits::default(), Duration::from_secs(120)).await;
    let mut client = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut client).await;

    assert!(welcome.get("iceServers").is_none());
}

/// 内置 STUN：没配 `PAIR_ICE_SERVERS` 时，用客户端连进来的主机名拼出 `stun:` 地址
#[tokio::test]
async fn the_welcome_advertises_the_builtin_stun_on_the_host_the_client_used() {
    let address = start_relay_full(
        Limits::default(),
        20,
        Duration::from_secs(120),
        None,
        Some(3479),
    )
    .await;
    let mut client = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut client).await;

    assert_eq!(
        welcome["iceServers"],
        serde_json::json!([{ "urls": ["stun:127.0.0.1:3479"] }])
    );
}

/// 部署者自己配了 `PAIR_ICE_SERVERS`（比如装了 coturn）时，以他的为准
#[tokio::test]
async fn configured_ice_servers_win_over_the_builtin_stun() {
    let ice_servers = serde_json::json!([{ "urls": ["stun:cat.example.com:3478"] }]);
    let address = start_relay_full(
        Limits::default(),
        20,
        Duration::from_secs(120),
        Some(ice_servers.clone()),
        Some(3479),
    )
    .await;
    let mut client = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut client).await;

    assert_eq!(welcome["iceServers"], ice_servers);
}

// ---------------------------------------------------------------------------
// 公益档（public tier）：别人借这台服务器打洞，但中继不替他们转发任何数据
// ---------------------------------------------------------------------------

/// 用公益密码连进来（并带上 `X-Bongo-Tier`，也就是新客户端的形状）
async fn connect_public(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    device_id: &str,
) -> Client {
    connect_with_server(address, room_id, token, "1", device_id, &public_token())
        .await
        .unwrap()
}

/// 老客户端的形状：**没有** `X-Bongo-Tier` 这个头
async fn connect_without_tier(
    address: SocketAddr,
    room_id: &str,
    token: &str,
    device_id: &str,
    server: &str,
) -> Result<Client, WsError> {
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();

    {
        let headers = request.headers_mut();

        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("x-bongo-client", device_id.parse().unwrap());
        headers.insert("x-bongo-protocol", "1".parse().unwrap());
        headers.insert("x-bongo-room", room_id.parse().unwrap());

        if !server.is_empty() {
            headers.insert("x-bongo-server", server.parse().unwrap());
        }
    }

    let (socket, _) = connect_async(request).await?;

    Ok(socket)
}

/// 等一条**应用帧**（跳过中间那些控制帧）
async fn next_binary(client: &mut Client) -> Vec<u8> {
    loop {
        let message = tokio::time::timeout(PATIENCE, client.next())
            .await
            .expect("等转发帧超时")
            .unwrap()
            .unwrap();

        if let Message::Binary(bytes) = message {
            return bytes.to_vec();
        }
    }
}

/// 这段时间内对端不该收到任何**应用帧**。
///
/// 不能直接用 `expect_silence`：发送方被 1008 关掉时，对端照例会收到一条「它离线了」的
/// 控制帧——那是应该发生的，这条用例要证明的只是「数据没被转发过去」。
async fn expect_no_binary(client: &mut Client, what: &str) {
    loop {
        match tokio::time::timeout(NEGATIVE_PATIENCE, client.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Binary(_)))) => panic!("{what} 收到了不该转发过去的应用帧"),
            Ok(Some(Ok(_))) => continue,
            Ok(_) => return,
        }
    }
}

/// 公益档的 welcome：明说自己是哪一档、额度只有信令那么大，而且**一个 `turn:` 都不给**
/// （它按流量计费，比带宽贵得多）
#[tokio::test]
async fn the_public_tier_is_announced_with_stun_only() {
    let ice_servers = serde_json::json!([
        { "urls": ["stun:cat.example.com:3478"] },
        {
            "urls": ["turn:cat.example.com:3478"],
            "username": "coturn-user",
            "credential": "coturn-pass"
        }
    ]);
    let mut settings = config(Limits::default(), 20, Duration::from_secs(120), None, None);

    settings.ice_servers = Some(ice_servers.clone());
    // 公益档要配一把钥匙才存在（上面那份 `config` 里只有完全档）
    settings
        .server_keys
        .push(ServerKey::new(Tier::Public, PUBLIC_PASSWORD));

    let address = start_relay_with_config(settings).await;

    // 部署者那一档：原样透传（连 TURN 凭据一起）
    let mut owner = connect_a(address, "aaaa").await;
    let welcome = next_json(&mut owner).await;

    assert_eq!(welcome["tier"], "full");
    assert_eq!(welcome["iceServers"], ice_servers);

    // 公益档：只留 stun，凭据一起摘掉
    let mut guest = connect_public(address, ROOM_B, TOKEN_B, "bbbb").await;
    let welcome = next_json(&mut guest).await;

    assert_eq!(welcome["type"], "server.welcome");
    assert_eq!(welcome["tier"], "public");
    assert_eq!(
        welcome["iceServers"],
        serde_json::json!([{ "urls": ["stun:cat.example.com:3478"] }])
    );
    assert_eq!(welcome["limits"]["framesPerSecond"], 12.0);
    assert_eq!(
        welcome["limits"]["bytesPerSecond"],
        DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND
    );
}

/// 配了公益密码、但名额是 0（`PAIR_MAX_PUBLIC_SESSIONS=0`）= 这一档**实际关掉**。
///
/// 关掉就要像「没有公益档」一样表现：`/health` 报 `publicTier:false`，拿着公益密码的人
/// 直接 403（「服务器密码不正确」），**不是** 503「会话已满」——那说的是一件没发生的事
/// （名额根本没被占满），还会让客户端按退避无限重试一台永远进不去的服务器。
#[tokio::test]
async fn a_switched_off_public_tier_rejects_its_password_and_reports_itself_as_off() {
    let address = start_relay_with_config(public_config(20, 0, 4, None)).await;

    let health = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(health.contains("\"publicTier\":false"), "实际：{health}");

    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", &public_token()).await,
        403
    );

    // 部署者那一档一字不变（同一个 Room 也照旧能进）
    let mut owner = connect_a(address, "aaaa").await;

    assert_eq!(next_json(&mut owner).await["tier"], "full");
}

/// 开着的时候 `/health` 要说 true，而且公益连接照样进得来（上面那条的对照）
#[tokio::test]
async fn an_enabled_public_tier_reports_itself_as_on() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;

    let health = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(health.contains("\"publicTier\":true"), "实际：{health}");

    let mut guest = connect_public(address, ROOM_B, TOKEN_B, "bbbb").await;

    assert_eq!(next_json(&mut guest).await["tier"], "public");
}

/// 多把钥匙（配置里 `;` 分隔）：同一档可以配好几把，每把都按自己那一档生效。
///
/// 这是「每把钥匙各给一个人」的用法：换人时只撤销一把，别人照旧。`/health` 只报把数
/// （完全档 / 公益档各几把），好让部署者确认自己没写漏。
#[tokio::test]
async fn extra_keys_keep_their_own_tier() {
    let mut settings = public_config(20, 10, 4, None);

    settings
        .server_keys
        .push(ServerKey::new(Tier::Full, SECOND_SERVER_PASSWORD));
    settings
        .server_keys
        .push(ServerKey::new(Tier::Public, SECOND_PUBLIC_PASSWORD));

    let address = start_relay_with_config(settings).await;
    let health = raw_request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

    assert!(
        health.contains("\"serverKeys\":{\"full\":2,\"public\":2}"),
        "实际：{health}"
    );

    // 第二把完全档钥匙：照旧能用中继兜底
    let mut owner = connect_with_server(
        address,
        ROOM_A,
        TOKEN_A,
        "1",
        "aaaa",
        &auth::derive_server_token(SECOND_SERVER_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(next_json(&mut owner).await["tier"], "full");

    // 第二把公益钥匙：照旧只是公益档
    let mut guest = connect_with_server(
        address,
        ROOM_B,
        TOKEN_B,
        "1",
        "bbbb",
        &auth::derive_server_token(SECOND_PUBLIC_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(next_json(&mut guest).await["tier"], "public");

    // 表里没有的钥匙：照旧 403（多把钥匙不会让门槛变松）
    assert_eq!(
        handshake_status_with_server(
            address,
            ROOM_C,
            TOKEN_C,
            "1",
            "cccc",
            &auth::derive_server_token("relay-tests-stranger-key")
        )
        .await,
        403
    );
}

/// 公益档只放行信令（kind 8）：塞数据帧会被 1008 关掉，而且**不会**转发到对面
#[tokio::test]
async fn a_public_connection_cannot_forward_data_frames() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;
    let mut first = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;

    next_json(&mut first).await;

    let mut second = connect_public(address, ROOM_A, TOKEN_A, "bbbb").await;

    assert_eq!(next_json(&mut second).await["peerOnline"], true);
    next_json(&mut first).await;

    // kind 4 = chat：部署者那一档照转发，公益档必须挡住
    first
        .send(Message::Binary(frame(4, 16).into()))
        .await
        .unwrap();

    assert_eq!(
        wait_close(&mut first).await,
        Some(close_code::PROTOCOL_ERROR)
    );
    expect_no_binary(&mut second, "公益档对面").await;

    // 换个 deviceId 重新进来，这次只发信令：照旧转发
    let mut third = connect_public(address, ROOM_A, TOKEN_A, "cccc").await;

    next_json(&mut third).await;

    let signal = frame(protocol::FRAME_KIND_SIGNAL, 16);

    third
        .send(Message::Binary(signal.clone().into()))
        .await
        .unwrap();

    assert_eq!(next_binary(&mut second).await, signal);
}

/// 公益档要求客户端**认得这一档**：老客户端拿着公益密码在握手就被挡住（426 = 请升级），
/// 而不是「连上之后被踢」或者「界面显示已连接、其实什么都通不了」
#[tokio::test]
async fn the_public_tier_needs_a_tier_aware_client() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;

    // 老客户端的形状：不带 `X-Bongo-Tier`
    let rejected = connect_without_tier(address, ROOM_A, TOKEN_A, "aaaa", &public_token()).await;

    match rejected {
        Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), 426),
        other => panic!("期望 426，实际 {other:?}"),
    }

    // 部署者那一档完全无视这个头：老客户端照旧进得来（也就照旧能连所有旧中继）
    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "aaaa", &server_token()).await,
        101
    );
}

/// 公益档的名额与部署者那一档**完全分开**
#[tokio::test]
async fn the_public_tier_does_not_consume_the_full_tier_slots() {
    // 两档各只有一组名额
    let address = start_relay_with_config(public_config(1, 1, 4, None)).await;

    // 两条连接都得**留着**：会话空掉名额就还回去了，那样这条用例就测不到东西
    let _guest = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;

    // 公益档占满之后，部署者自己的名额仍然空着
    let _owner = connect_with_server(address, ROOM_B, TOKEN_B, "1", "bbbb", &server_token())
        .await
        .unwrap();

    // 而第二个**公益**会话才该被拒（满的是它自己那一档）
    assert_eq!(
        handshake_status_with_server(address, ROOM_C, TOKEN_C, "1", "cccc", &public_token()).await,
        503
    );
}

/// 同一个 IP 的公益会话数有上限；但**同一个会话的第二个人**照旧进得来
/// （不然同一个 NAT 下面的一对用户会被自己挡住）
#[tokio::test]
async fn the_public_per_ip_limit_only_blocks_new_rooms() {
    let address = start_relay_with_config(public_config(20, 20, 1, None)).await;

    let _first = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;
    // 同一个会话的第二个人不该被每 IP 限额挡住
    let _second = connect_public(address, ROOM_A, TOKEN_A, "bbbb").await;

    assert_eq!(
        handshake_status_with_server(address, ROOM_B, TOKEN_B, "1", "cccc", &public_token()).await,
        429
    );
}

/// 客户端伪造 `X-Forwarded-For` 赢不了：中继只认**最后一项**，那才是我们那一跳（Caddy）写
/// 上去的地址。
///
/// 关键在于这个头会被拆成**多行**（Go 的 `net/http` 给已存在的头 append 值就是写成另一行），
/// 所以「只看第一行」等于把客户端伪造的那一行当成了真实 IP：每 IP 限额被直接绕开，一台机器
/// 换着假 IP 就能把公益名额吃满，还能把账记到别人头上让别人吃 429。
#[tokio::test]
async fn a_forged_forwarded_for_line_cannot_win() {
    // `PAIR_TRUST_PROXY=1`（域名模式的 compose 默认就是它），每 IP 只给一组公益名额。
    // 对端是本机回环——那正是「我们那一跳」的形状，所以这份头才会被信。
    let config = Config {
        trust_proxy: true,
        ..public_config(20, 10, 1, None)
    };
    let address = start_relay_with_config(config).await;

    // 伪造行在前、可信行在后：必须按**可信行**记账，所以连得上（这条连接要一直留着，
    // 会话空掉名额就还回去了）
    let mut first = connect_with_forwarded(
        address,
        ROOM_A,
        TOKEN_A,
        "1",
        "aaaa",
        &public_token(),
        &["198.51.100.7", "203.0.113.5"],
    )
    .await
    .unwrap();

    next_json(&mut first).await;

    // 同一台机器换一个伪造值再来：真实 IP 没变，所以被每 IP 限额挡下
    match connect_with_forwarded(
        address,
        ROOM_B,
        TOKEN_B,
        "1",
        "bbbb",
        &public_token(),
        &["192.0.2.99", "203.0.113.5"],
    )
    .await
    {
        Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), 429),
        Ok(_) => panic!("伪造的 X-Forwarded-For 绕过了每 IP 限额"),
        Err(other) => panic!("期望 429，实际 {other:?}"),
    }

    // 换一个可信值就是另一个 IP：进得来（证明上一条不是因为别的原因被挡）
    let mut third = connect_with_forwarded(
        address,
        ROOM_C,
        TOKEN_C,
        "1",
        "cccc",
        &public_token(),
        &["192.0.2.99", "203.0.113.6"],
    )
    .await
    .unwrap();

    next_json(&mut third).await;
}

/// 两边填了不同类型的密码却进了同一个会话：明确报冲突（409），而不是让一个人有中继兜底、
/// 另一个人什么都没有
#[tokio::test]
async fn mixing_the_two_passwords_in_one_room_is_a_conflict() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;

    let _guest = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;

    assert_eq!(
        handshake_status_with_server(address, ROOM_A, TOKEN_A, "1", "bbbb", &server_token()).await,
        409
    );
}

/// 公益档的单帧上限（64 KiB）：超过就 1009，别让它变成一条夹带通道
#[tokio::test]
async fn an_oversized_public_frame_closes_with_1009() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;
    let mut client = connect_public(address, ROOM_A, TOKEN_A, "aaaa").await;

    next_json(&mut client).await;

    // 刚好越过 64 KiB 一字节（帧头也算在里面）
    let oversized = frame(
        protocol::FRAME_KIND_SIGNAL,
        protocol::MAX_PUBLIC_FRAME_SIZE - protocol::FRAME_HEADER_SIZE + 1,
    );

    let _ = client.send(Message::Binary(oversized.into())).await;

    assert_eq!(wait_close(&mut client).await, Some(close_code::TOO_LARGE));
}

/// 公益档的空闲回收：窗口内一条入站消息都没有就断开（4005）。
///
/// 它是**空闲回收器**，不是「打洞截止时间」：中继看不到 DataChannel 有没有建起来，
/// 所以判据只能是「这条连接还有没有动静」。
#[tokio::test]
async fn the_public_window_closes_an_idle_connection_with_4005() {
    let address =
        start_relay_with_config(public_config(20, 10, 4, Some(Duration::from_millis(300)))).await;

    // 部署者那一档没有这个窗口（用另一个会话，免得和下面那条公益连接撞档位）
    let mut owner = connect_with_server(address, ROOM_B, TOKEN_B, "1", "aaaa", &server_token())
        .await
        .unwrap();

    next_json(&mut owner).await;

    let quiet = tokio::time::timeout(Duration::from_millis(700), owner.next()).await;

    assert!(quiet.is_err(), "部署者那一档不该被公益档的空闲窗口收掉");

    // 公益档：静着不动就会被回收
    let mut guest = connect_public(address, ROOM_A, TOKEN_A, "bbbb").await;

    next_json(&mut guest).await;

    assert_eq!(wait_close(&mut guest).await, Some(close_code::IDLE));
}

/// 完全档同样有空闲回收（`PAIR_FULL_WINDOW_SECS`），关闭码与公益档共用 `4005`。
///
/// 它挡的是**僵尸连接**：对端机器睡眠 / 网线被拔之后，TCP 可能几小时都不报错，于是一条
/// 什么都没在传的连接会一直占着会话名额与一条连接额度。诚实客户端每 60 秒一次 WebSocket
/// Ping（`manager.rs` 的 ticker，窗口藏起来也照发），所以日常根本碰不到它。
#[tokio::test]
async fn the_full_window_reaps_an_idle_connection_with_4005() {
    let config = Config {
        // 公益档关掉：这一条只看完全档那一个窗口
        max_public_sessions: 0,
        full_window: Some(Duration::from_millis(300)),
        ..config(Limits::default(), 20, Duration::from_secs(120), None, None)
    };
    let address = start_relay_with_config(config).await;
    let mut owner = connect_with_server(address, ROOM_B, TOKEN_B, "1", "aaaa", &server_token())
        .await
        .unwrap();

    next_json(&mut owner).await;

    assert_eq!(wait_close(&mut owner).await, Some(close_code::IDLE));
}

/// 回归：公益档一加，部署者那一档的转发照旧（kind 1 与 kind 6 都能过）
#[tokio::test]
async fn the_full_tier_is_unchanged_by_the_public_tier() {
    let address = start_relay_with_config(public_config(20, 10, 4, None)).await;
    let mut first = connect_a(address, "aaaa").await;

    assert_eq!(next_json(&mut first).await["tier"], "full");

    let mut second = connect_a(address, "bbbb").await;

    next_json(&mut second).await;
    next_json(&mut first).await;

    for kind in [1u8, protocol::FRAME_KIND_TRANSFER_CHUNK] {
        let sent = frame(kind, 32);

        first
            .send(Message::Binary(sent.clone().into()))
            .await
            .unwrap();

        assert_eq!(next_binary(&mut second).await, sent);
    }
}

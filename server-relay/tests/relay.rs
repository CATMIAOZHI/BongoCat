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

use bongocat_pair_relay::relay::{Relay, RelayOptions};
use bongocat_pair_relay::server::{self, Config};
use bongocat_pair_relay::{
    auth,
    protocol::{self, close_code, Limits, DEFAULT_PUBLIC_MAX_BYTES_PER_SECOND},
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
        server_verifier: auth::server_verifier(SERVER_PASSWORD),
        public_limits: defaults.public_limits,
        max_public_sessions: defaults.max_public_sessions,
        max_public_per_ip: defaults.max_public_per_ip,
        public_window: defaults.public_window,
        public_verifier: None,
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
    Config {
        public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
        max_public_sessions,
        max_public_per_ip,
        public_window: window,
        ..config(Limits::default(), max_sessions, Duration::from_secs(120), None, None)
    }
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
    let address = start_relay_with_config(Config {
        ice_servers: Some(ice_servers.clone()),
        public_verifier: Some(auth::server_verifier(PUBLIC_PASSWORD)),
        ..config(
            Limits::default(),
            20,
            Duration::from_secs(120),
            None,
            None,
        )
    })
    .await;

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
    assert_eq!(welcome["limits"]["framesPerSecond"], 10.0);
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

    assert_eq!(wait_close(&mut first).await, Some(close_code::PROTOCOL_ERROR));
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
        handshake_status_with_server(address, ROOM_C, TOKEN_C, "1", "cccc", &public_token())
            .await,
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
        handshake_status_with_server(address, ROOM_B, TOKEN_B, "1", "cccc", &public_token())
            .await,
        429
    );
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

    assert_eq!(wait_close(&mut guest).await, Some(close_code::PUBLIC_WINDOW));
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

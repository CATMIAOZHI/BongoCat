//! HTTP 路由、鉴权与配置。中继的会话逻辑在 `relay.rs`。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tokio::net::{TcpListener, TcpStream};

use crate::auth;
use crate::http::{read_request_head, write_response, write_response_with_headers};
use crate::protocol::{
    is_valid_device_id, is_valid_room_id, Limits, DEFAULT_MAX_BYTES_PER_SECOND,
    DEFAULT_MAX_CHUNKS_PER_SECOND, DEFAULT_MAX_FRAMES_PER_SECOND, DEFAULT_MAX_SESSIONS,
    DEFAULT_STALE_AFTER_MS, HEADER_AUTHORIZATION, HEADER_CLIENT, HEADER_PROTOCOL, HEADER_ROOM,
    HEALTH_PATH, PROTOCOL_VERSION, WEBSOCKET_VERSION, WS_PATH,
};
use crate::relay::{Relay, RoomRejection};

/// 请求头必须在这个时间内读完：只发一个连接、永远不发请求头的客户端不该占住一个任务
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `Sec-WebSocket-Key` 的合法性（RFC 6455 §4.2.1：16 字节的 base64）
fn is_valid_websocket_key(value: &str) -> bool {
    STANDARD
        .decode(value.trim())
        .is_ok_and(|bytes| bytes.len() == 16)
}

/// 运行期配置。全部可以用环境变量覆盖（见 `load_config`）。
///
/// 多会话（§6）下这里**没有 Pair Secret、也没有 PAIR_AUTH_TOKEN**：一套服务器服务
/// 的是所有自带联机密钥的用户，鉴权退化成「同一个 Room 的人拿的 token 摘要一致」。
pub struct Config {
    /// 监督下发给客户端的限流额度，同时就是中继自己的桶容量
    pub limits: Limits,
    /// 同时承载的双人会话数上限（`PAIR_MAX_SESSIONS`）
    pub max_sessions: usize,
    /// 多久没有消息的连接可以被新连接顶替
    pub stale_after: Duration,
    /// `/ws` 的 `server.welcome` 里附带的 ICE 服务器（可选，原样透传）
    pub ice_servers: Option<serde_json::Value>,
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_f64(name: &str, default: f64) -> Result<f64, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0 && value.is_finite())
            .ok_or_else(|| format!("{name} 必须是大于 0 的数字，实际是 {text:?}")),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<u64>()
            .map_err(|_| format!("{name} 必须是非负整数，实际是 {text:?}")),
    }
}

/// `PAIR_MAX_SESSIONS` 必须 ≥ 1。
///
/// 0 会让整套服务器一个人都进不来（任何新会话都被判满），那几乎一定是配置事故；
/// 与其默默接受一个「永远 503」的部署，不如启动就报错。
fn env_positive_usize(name: &str, default: usize) -> Result<usize, String> {
    match env_non_empty(name) {
        None => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|value| *value >= 1)
            .ok_or_else(|| format!("{name} 必须是大于 0 的整数，实际是 {text:?}")),
    }
}

/// 读环境变量组装配置。
pub fn load_config() -> Result<Config, String> {
    let limits = Limits {
        frames_per_second: env_f64("PAIR_MAX_FRAMES_PER_SECOND", DEFAULT_MAX_FRAMES_PER_SECOND)?,
        chunks_per_second: env_f64("PAIR_MAX_CHUNKS_PER_SECOND", DEFAULT_MAX_CHUNKS_PER_SECOND)?,
        bytes_per_second: env_f64("PAIR_MAX_BYTES_PER_SECOND", DEFAULT_MAX_BYTES_PER_SECOND)?,
    };

    let ice_servers = match env_non_empty("PAIR_ICE_SERVERS") {
        None => None,
        Some(text) => Some(
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|error| format!("PAIR_ICE_SERVERS 不是合法 JSON: {error}"))?,
        ),
    };

    Ok(Config {
        limits,
        max_sessions: env_positive_usize("PAIR_MAX_SESSIONS", DEFAULT_MAX_SESSIONS)?,
        stale_after: Duration::from_millis(env_u64("PAIR_STALE_AFTER_MS", DEFAULT_STALE_AFTER_MS)?),
        ice_servers,
    })
}

pub async fn listen_address() -> Result<String, String> {
    Ok(env_non_empty("PAIR_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_string()))
}

/// 接受连接，直到监听器出错。
///
/// 只收 `relay`：`Config` 里的每一项都已经被折进 `Relay` 了（限流、容量、陈旧判定、
/// ICE），会话层之外没有第二份真相。
pub async fn serve(listener: TcpListener, relay: Arc<Relay>) {
    loop {
        let accepted = listener.accept().await;

        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("接受连接失败：{error}");

                continue;
            }
        };

        // 实时帧很小，禁用 Nagle 才不会让状态卡在缓冲里
        let _ = stream.set_nodelay(true);

        let relay = Arc::clone(&relay);

        tokio::spawn(async move {
            if let Err(error) = handle(stream, peer, relay).await {
                // 客户端断线是常态：记一行即可，不影响其它连接
                eprintln!("连接 {peer} 结束：{error}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, peer: SocketAddr, relay: Arc<Relay>) -> Result<(), String> {
    // 握手超时：只连不发（或慢慢发）的客户端不能无限占着一个任务
    let head = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_request_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => return Err("握手超时".into()),
    };

    if head.path() == HEALTH_PATH {
        // §28：只说「我活着、协议是 1」。**不暴露**任何 Room、deviceId 或密钥信息
        let body =
            format!("{{\"ok\":true,\"protocol\":{PROTOCOL_VERSION},\"mode\":\"multi-pair\"}}");

        return write_response(&mut stream, 200, "OK", "application/json", &body)
            .await
            .map_err(|error| error.to_string());
    }

    if head.path() != WS_PATH {
        return write_response(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "not found",
        )
        .await
        .map_err(|error| error.to_string());
    }

    if !head
        .header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        return write_response(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            "expected websocket upgrade",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // 版本不对时按 RFC 6455 §4.4 回 426 并带上支持的版本
    if head.header("sec-websocket-version") != Some(WEBSOCKET_VERSION) {
        return write_response_with_headers(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            &format!("Sec-WebSocket-Version: {WEBSOCKET_VERSION}\r\n"),
            "unsupported websocket version",
        )
        .await
        .map_err(|error| error.to_string());
    }

    if !head
        .header("sec-websocket-key")
        .is_some_and(is_valid_websocket_key)
    {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid websocket key",
        )
        .await
        .map_err(|error| error.to_string());
    }

    let expected_protocol = PROTOCOL_VERSION.to_string();

    if head.header(HEADER_PROTOCOL) != Some(expected_protocol.as_str()) {
        return write_response(
            &mut stream,
            426,
            "Upgrade Required",
            "text/plain; charset=utf-8",
            "unsupported protocol",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §4：中继要靠 Room 才能找到「这次连接属于哪个会话」，所以它排在鉴权前面。
    // 格式规则是公开的（43 个 base64url 字符），不是秘密，400 这里不泄露任何东西。
    let room_id = head
        .header(HEADER_ROOM)
        .unwrap_or_default()
        .trim()
        .to_string();

    if !is_valid_room_id(&room_id) {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid room id",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // 空的 Authorization 连摘要都算不出来：先按未鉴权拒掉
    let token = auth::bearer_token(head.header(HEADER_AUTHORIZATION));

    if token.is_empty() {
        return write_response(
            &mut stream,
            401,
            "Unauthorized",
            "text/plain; charset=utf-8",
            "authentication failed",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §8 / §9 / §27：容量与密钥判定都在升级之前完成，客户端才能按状态码区分
    // 「联机密钥不正确」（401）与「服务器会话已满」（503）。
    let reservation = match relay.reserve(&room_id, auth::auth_verifier(&token)).await {
        Ok(reservation) => reservation,
        Err(RoomRejection::AuthMismatch) => {
            return write_response(
                &mut stream,
                401,
                "Unauthorized",
                "text/plain; charset=utf-8",
                "authentication failed",
            )
            .await
            .map_err(|error| error.to_string());
        }
        Err(RoomRejection::Capacity) => {
            return write_response(
                &mut stream,
                503,
                "Service Unavailable",
                "text/plain; charset=utf-8",
                "server capacity reached",
            )
            .await
            .map_err(|error| error.to_string());
        }
    };

    // 先通过房间校验再看 deviceId：未通过鉴权的请求不该能探测 deviceId 的合法性
    // （新建会话的情况没有共享凭据可验，这里只能保证「已有会话」不被探测）。
    // 归一成小写：同一个 UUID 用大写重连时不能被当成第三个人而自锁
    let device_id = head
        .header(HEADER_CLIENT)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if !is_valid_device_id(&device_id) {
        // 名额已经占上了，这里必须还回去，否则一个拼错 deviceId 的客户端会永久占住一个会话位
        relay.release(reservation).await;

        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "invalid client id",
        )
        .await
        .map_err(|error| error.to_string());
    }

    // §29：日志只写 Room 指纹，不写完整 ROOM_ID
    println!(
        "{peer} 已通过鉴权（device {device_id}，room {}）",
        auth::room_fingerprint(&room_id)
    );

    relay.serve(stream, head, device_id, reservation).await
}

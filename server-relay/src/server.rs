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
    is_valid_device_id, Limits, DEFAULT_MAX_BYTES_PER_SECOND, DEFAULT_MAX_CHUNKS_PER_SECOND,
    DEFAULT_MAX_FRAMES_PER_SECOND, DEFAULT_STALE_AFTER_MS, HEADER_AUTHORIZATION, HEADER_CLIENT,
    HEADER_PROTOCOL, HEALTH_PATH, PROTOCOL_VERSION, WEBSOCKET_VERSION, WS_PATH,
};
use crate::relay::Relay;

/// 请求头必须在这个时间内读完：只发一个连接、永远不发请求头的客户端不该占住一个任务
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `Sec-WebSocket-Key` 的合法性（RFC 6455 §4.2.1：16 字节的 base64）
fn is_valid_websocket_key(value: &str) -> bool {
    STANDARD
        .decode(value.trim())
        .is_ok_and(|bytes| bytes.len() == 16)
}

/// 运行期配置。除了 token 之外都可以用环境变量覆盖（见 `load_config`）。
pub struct Config {
    /// 期望的 `Authorization: Bearer <token>`
    pub token: String,
    /// 监督下发给客户端的限流额度，同时就是中继自己的桶容量
    pub limits: Limits,
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

/// 读环境变量组装配置。
///
/// `PAIR_AUTH_TOKEN` 与 `PAIR_SECRET` 二选一：直接给 token 最省事；给 Pair Secret
/// 时中继自己按 R17 派生，用户只需要保存一个值（推荐，因为客户端填的也是它）。
pub fn load_config() -> Result<Config, String> {
    let token = match (
        env_non_empty("PAIR_AUTH_TOKEN"),
        env_non_empty("PAIR_SECRET"),
    ) {
        (Some(token), _) => token,
        (None, Some(secret)) => {
            let decoded = auth::decode_pair_secret(&secret)?;

            auth::derive_auth_token(&decoded)
        }
        (None, None) => {
            return Err(
                "必须设置 PAIR_AUTH_TOKEN，或者设置 PAIR_SECRET（中继会自己派生 token）".into(),
            );
        }
    };

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
        token,
        limits,
        stale_after: Duration::from_millis(env_u64("PAIR_STALE_AFTER_MS", DEFAULT_STALE_AFTER_MS)?),
        ice_servers,
    })
}

pub async fn listen_address() -> Result<String, String> {
    Ok(env_non_empty("PAIR_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_string()))
}

/// 接受连接，直到监听器出错。
pub async fn serve(listener: TcpListener, config: Arc<Config>, relay: Arc<Relay>) {
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

        let config = Arc::clone(&config);
        let relay = Arc::clone(&relay);

        tokio::spawn(async move {
            if let Err(error) = handle(stream, peer, config, relay).await {
                // 客户端断线是常态：记一行即可，不影响其它连接
                eprintln!("连接 {peer} 结束：{error}");
            }
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    config: Arc<Config>,
    relay: Arc<Relay>,
) -> Result<(), String> {
    // 握手超时：只连不发（或慢慢发）的客户端不能无限占着一个任务
    let head = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_request_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => return Err("握手超时".into()),
    };

    if head.path() == HEALTH_PATH {
        let body = format!("{{\"ok\":true,\"protocol\":{PROTOCOL_VERSION}}}");

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

    // 先鉴权再看 deviceId：未鉴权的请求不该能探测 deviceId 的合法性
    let token = auth::bearer_token(head.header(HEADER_AUTHORIZATION));

    if token.is_empty() || !auth::constant_time_eq(token.as_bytes(), config.token.as_bytes()) {
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

    // 归一成小写：同一个 UUID 用大写重连时不能被当成第三个人而自锁
    let device_id = head
        .header(HEADER_CLIENT)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if !is_valid_device_id(&device_id) {
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

    println!("{peer} 已通过鉴权（device {device_id}）");

    relay.serve(stream, head, device_id).await
}

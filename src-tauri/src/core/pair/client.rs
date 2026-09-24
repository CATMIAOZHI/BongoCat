//! WebSocket 连接：URL 归一化、升级请求、错误信息翻译。
//!
//! 只负责「连上」，不负责重连、状态机与加解密（那些在 manager.rs）。

#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
#[cfg(test)]
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::crypto::ROOM_ID_LENGTH;
use super::protocol::PROTOCOL_VERSION;

pub type PairSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 连接失败的原因。
///
/// `fatal` 表示「重试也不会好」——联机密钥不对、协议版本不对、地址路径写错。
/// 中继侧一旦给出这类答案，继续按 1/2/5/10/30 秒退避重连只会刷日志，所以
/// manager 会停在 `Error` 状态等用户处理。
///
/// **容量满不是 fatal**（§27 的「请稍后再试」就是这个意思）：名额由别的会话释放，
/// 按退避重连正是应该发生的事，用户不需要手动点「立即连接」。
#[derive(Debug, Clone)]
pub struct PairFailure {
    pub message: String,
    pub fatal: bool,
}

impl PairFailure {
    fn transient(message: String) -> Self {
        Self {
            message,
            fatal: false,
        }
    }

    fn fatal(message: String) -> Self {
        Self {
            message,
            fatal: true,
        }
    }
}

/// 把用户填的地址补全成 `/ws` 升级地址。
///
/// 用户填的通常是部署输出里的 `https://<worker>.workers.dev`，但 WebSocket 客户端
/// 只接受 `ws` / `wss` scheme（`http://` 会直接报 URL scheme not supported），
/// 所以这里必须把 scheme 换掉。
///
/// 没写 scheme 时（§23）：域名按 `wss://` 处理；**裸 IP 与本机地址按 `ws://` 处理**——
/// 自己用 `docker-compose.direct.yml` 起的服务器没有证书，猜 `wss://` 只会让用户拿到
/// 一个看不懂的 TLS 错误。显式写了 scheme 就完全尊重用户输入。
pub fn build_upgrade_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_end_matches('/');

    if trimmed.is_empty() {
        return Err("服务器地址不能为空".into());
    }

    let with_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if looks_like_plaintext_host(trimmed) {
        format!("ws://{trimmed}")
    } else {
        format!("wss://{trimmed}")
    };

    if with_scheme.ends_with("/ws") {
        Ok(with_scheme)
    } else {
        Ok(format!("{with_scheme}/ws"))
    }
}

/// 地址是不是明文（没有 TLS）：显式 `http://` / `ws://`，或者没写 scheme 的裸 IP / 本机地址。
///
/// 只用于 UI 上那条非阻塞提醒——**绝不阻止连接**（自建服务器 + 内网 / 测试环境
/// 本来就允许明文）。
pub fn is_plaintext_endpoint(input: &str) -> bool {
    let trimmed = input.trim();

    trimmed.starts_with("http://")
        || trimmed.starts_with("ws://")
        || (!trimmed.contains("://") && looks_like_plaintext_host(trimmed))
}

/// 没有 scheme 时，主机部分是不是「不可能有证书」的那一类：IPv4 字面量、`[IPv6]`、`localhost`。
///
/// `localhost` 也算进来：自己在本机跑中继（`PAIR_LISTEN=127.0.0.1:8811`）是最常见的第一
/// 次试用方式，猜 `wss://` 只会给用户一个 `127.0.0.1` 上的 TLS 错误。
fn looks_like_plaintext_host(input: &str) -> bool {
    let host = input.split('/').next().unwrap_or_default();

    if host.starts_with('[') {
        return host.contains(']');
    }

    let ipv4 = host.split(':').next().unwrap_or_default();

    if ipv4.eq_ignore_ascii_case("localhost") {
        return true;
    }

    let mut parts = 0;

    for part in ipv4.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }

        parts += 1;
    }

    parts == 4
}

/// `ROOM_ID` 的合法性（服务端是同一套规则的上界：非空、≤ 64、`[A-Za-z0-9_-]`）。
///
/// 客户端只可能派生出一个值，所以这里**连长度都按定长校验**：一旦派生漂移（比如
/// 有人改了 `ROOM_INFO` 又忘了测试向量），这里会立刻给出 fatal，而不是把两个人
/// 悄悄拆进两个房间。
fn is_valid_room_id(room_id: &str) -> bool {
    room_id.len() == ROOM_ID_LENGTH
        && room_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub async fn connect(
    relay_url: &str,
    room_id: &str,
    auth_token: &str,
    device_id: &str,
) -> Result<PairSocket, PairFailure> {
    let request = build_request(relay_url, room_id, auth_token, device_id)?;

    let (socket, _response) = connect_async(request)
        .await
        .map_err(describe_connect_error)?;

    Ok(socket)
}

/// 组装升级请求：四个头一次写完，缺一个中继都会拒绝。
///
/// 单独抽出来是为了能被单测直接断言（§31：请求**一定**包含 `X-Bongo-Room`）。
fn build_request(
    relay_url: &str,
    room_id: &str,
    auth_token: &str,
    device_id: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, PairFailure> {
    let url = build_upgrade_url(relay_url).map_err(PairFailure::fatal)?;

    if !is_valid_room_id(room_id) {
        return Err(PairFailure::fatal(
            "联机会话标识不合法：请重新填写联机密钥".to_string(),
        ));
    }

    let mut request = url
        .into_client_request()
        .map_err(|err| PairFailure::fatal(format!("服务器地址不合法: {err}")))?;

    {
        let headers = request.headers_mut();
        let authorization = HeaderValue::from_str(&format!("Bearer {auth_token}"))
            .map_err(|_| PairFailure::fatal("联机密钥含非法字符".to_string()))?;
        let client = HeaderValue::from_str(device_id)
            .map_err(|_| PairFailure::fatal("设备标识含非法字符".to_string()))?;
        let protocol = HeaderValue::from_str(&PROTOCOL_VERSION.to_string())
            .map_err(|_| PairFailure::fatal("协议版本不合法".to_string()))?;
        let room = HeaderValue::from_str(room_id)
            .map_err(|_| PairFailure::fatal("联机会话标识含非法字符".to_string()))?;

        headers.insert("authorization", authorization);
        headers.insert("x-bongo-client", client);
        headers.insert("x-bongo-protocol", protocol);
        // §4：中继只按它分组，拿不到 Pair Secret，也拿不到 E2EE 根密钥
        headers.insert("x-bongo-room", room);
    }

    Ok(request)
}

/// 只给 `e2e.rs` 的真实中继测试直接用；生产路径由 manager 自己驱动 socket
#[cfg(test)]
pub async fn send_message(socket: &mut PairSocket, message: Message) -> Result<(), String> {
    socket
        .send(message)
        .await
        .map_err(|err| format!("发送失败: {err}"))
}

#[cfg(test)]
pub async fn next_message(socket: &mut PairSocket) -> Option<Result<Message, WsError>> {
    socket.next().await
}

fn describe_connect_error(error: WsError) -> PairFailure {
    match error {
        WsError::Http(response) => {
            let status = response.status();

            match status.as_u16() {
                401 => PairFailure::fatal("联机密钥不正确：请与对方核对是否完全相同".to_string()),
                426 => PairFailure::fatal("两边版本不一致：请把它们都升级到最新版".to_string()),
                // §1：别让用户去理解 deviceId / Room —— 说能做什么就行
                400 => PairFailure::fatal(
                    "服务器拒绝了这次连接：请检查服务器地址与联机密钥是否和对方完全一致"
                        .to_string(),
                ),
                404 => PairFailure::fatal("服务器地址路径不对：应该指向 /ws".to_string()),
                // §27：容量按「双人联机会话数」算，满了不该显示成一串 HTTP 码；
                // 也不该 fatal —— 「请稍后再试」意味着名额一被释放就该自己进去
                503 => PairFailure::transient("服务器双人联机会话已满，请稍后再试".to_string()),
                _ => PairFailure::transient(format!(
                    "服务器返回 HTTP {}，稍后自动重试",
                    status.as_u16()
                )),
            }
        }
        WsError::Url(err) => PairFailure::fatal(format!("服务器地址不合法: {err}")),
        WsError::Io(err) => PairFailure::transient(format!("连接失败: {err}")),
        other => PairFailure::transient(format!("连接失败: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pair::crypto::{decode_pair_secret, derive_auth_token, derive_room_id};

    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

    fn room_id() -> String {
        derive_room_id(&decode_pair_secret(SECRET).unwrap())
    }

    #[test]
    fn normalizes_relay_urls() {
        assert_eq!(
            build_upgrade_url("https://example.workers.dev").unwrap(),
            "wss://example.workers.dev/ws"
        );
        assert_eq!(
            build_upgrade_url("example.workers.dev/").unwrap(),
            "wss://example.workers.dev/ws"
        );
        assert_eq!(
            build_upgrade_url("wss://example.workers.dev/ws").unwrap(),
            "wss://example.workers.dev/ws"
        );
        assert_eq!(
            build_upgrade_url("  http://127.0.0.1:8787/  ").unwrap(),
            "ws://127.0.0.1:8787/ws"
        );
        assert_eq!(
            build_upgrade_url("https://example.workers.dev/ws").unwrap(),
            "wss://example.workers.dev/ws"
        );
        assert!(build_upgrade_url("   ").is_err());
    }

    /// §23 / §31：裸 IP 按明文处理（自建 direct 模式没有证书），域名仍然默认 wss
    #[test]
    fn bare_addresses_default_to_the_scheme_they_can_actually_serve() {
        assert_eq!(
            build_upgrade_url("cat.example.com").unwrap(),
            "wss://cat.example.com/ws"
        );
        assert_eq!(
            build_upgrade_url("127.0.0.1:8787").unwrap(),
            "ws://127.0.0.1:8787/ws"
        );
        assert_eq!(
            build_upgrade_url("192.168.1.10").unwrap(),
            "ws://192.168.1.10/ws"
        );
        assert_eq!(
            build_upgrade_url("[::1]:8080").unwrap(),
            "ws://[::1]:8080/ws"
        );
        // 显式 scheme 一律尊重
        assert_eq!(
            build_upgrade_url("https://127.0.0.1:8443").unwrap(),
            "wss://127.0.0.1:8443/ws"
        );
        assert_eq!(
            build_upgrade_url("wss://cat.example.com").unwrap(),
            "wss://cat.example.com/ws"
        );
        // 「三段数字」不是 IP，别把域名误判成明文
        assert_eq!(build_upgrade_url("1.2.3").unwrap(), "wss://1.2.3/ws");
    }

    /// 在本机自建中继（`localhost:8811`）时同样按明文处理，别猜 `wss://`
    #[test]
    fn the_local_host_also_defaults_to_plaintext() {
        assert_eq!(
            build_upgrade_url("localhost:8811").unwrap(),
            "ws://localhost:8811/ws"
        );
        assert_eq!(
            build_upgrade_url("LOCALHOST:8080").unwrap(),
            "ws://LOCALHOST:8080/ws"
        );
        assert!(is_plaintext_endpoint("localhost:8811"));
        // 只认主机名精确相等，别把「含 localhost 的域名」也算进来
        assert_eq!(
            build_upgrade_url("localhost.example.com").unwrap(),
            "wss://localhost.example.com/ws"
        );
        assert!(!is_plaintext_endpoint("localhost.example.com"));
        // 显式 scheme 仍然优先
        assert_eq!(
            build_upgrade_url("https://localhost:8443").unwrap(),
            "wss://localhost:8443/ws"
        );
    }

    #[test]
    fn plaintext_endpoints_are_only_flagged_not_refused() {
        assert!(is_plaintext_endpoint("http://cat.example.com"));
        assert!(is_plaintext_endpoint("ws://cat.example.com"));
        assert!(is_plaintext_endpoint("127.0.0.1:8787"));
        assert!(is_plaintext_endpoint("[::1]:8080"));
        assert!(!is_plaintext_endpoint("cat.example.com"));
        assert!(!is_plaintext_endpoint("https://cat.example.com"));
        assert!(!is_plaintext_endpoint("wss://cat.example.com"));
    }

    /// §31 点名要求的三条用户可见文案：401 / 503 走 `client.rs`，4003 走 `manager.rs`。
    ///
    /// 这里把状态码 → 文案 → 是否重试一次钉死：401 是「密钥不对」，重试没有意义，
    /// 必须 fatal；503 是「名额满了」，名额会被别人释放，必须能自动重试。
    #[test]
    fn http_status_codes_become_readable_messages() {
        let http = |status: u16| {
            let response = tokio_tungstenite::tungstenite::http::Response::builder()
                .status(status)
                .body(None)
                .unwrap();

            WsError::Http(Box::new(response))
        };

        let unauthorized = describe_connect_error(http(401));

        assert_eq!(
            unauthorized.message,
            "联机密钥不正确：请与对方核对是否完全相同"
        );
        assert!(unauthorized.fatal, "密钥不对时重试没有意义");

        let full = describe_connect_error(http(503));

        assert_eq!(full.message, "服务器双人联机会话已满，请稍后再试");
        assert!(!full.fatal, "名额会被释放，应当按退避自动重试");

        // 其余状态码也不能把裸 HTTP 码直接甩给用户
        assert_eq!(
            describe_connect_error(http(426)).message,
            "两边版本不一致：请把它们都升级到最新版"
        );
        assert_eq!(
            describe_connect_error(http(404)).message,
            "服务器地址路径不对：应该指向 /ws"
        );
        // §1：400 不能把 deviceId / Room 这类内部词直接甩给用户
        let bad_request = describe_connect_error(http(400));

        assert_eq!(
            bad_request.message,
            "服务器拒绝了这次连接：请检查服务器地址与联机密钥是否和对方完全一致"
        );
        assert!(bad_request.fatal);
        // 没见过的状态码才回落成 HTTP 码，并且按可重试处理
        let unknown = describe_connect_error(http(500));

        assert_eq!(unknown.message, "服务器返回 HTTP 500，稍后自动重试");
        assert!(!unknown.fatal);
        // 连接层错误（断网、超时）是可重试的
        assert!(!describe_connect_error(WsError::ConnectionClosed).fatal);
    }

    /// §31：请求一定带上 `X-Bongo-Room`，而且四个头都不能少
    #[test]
    fn every_upgrade_request_carries_the_room_header() {
        let request = build_request(
            "cat.example.com",
            &room_id(),
            &derive_auth_token(&decode_pair_secret(SECRET).unwrap()),
            "device-1",
        )
        .unwrap();
        let headers = request.headers();

        assert_eq!(headers["x-bongo-room"], room_id());
        assert!(
            headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("Bearer ")
        );
        assert_eq!(headers["x-bongo-client"], "device-1");
        assert_eq!(headers["x-bongo-protocol"], "1");
        assert_eq!(request.uri().to_string(), "wss://cat.example.com/ws");
    }

    /// 派生漂移 / 有人塞了别的值时必须是 fatal，不能带着非法 Room 去连接
    #[test]
    fn malformed_room_ids_are_fatal() {
        for broken in [
            "",
            "short",
            // 长度不是 43（少一位 / 多一位）
            &"a".repeat(ROOM_ID_LENGTH - 1),
            &"a".repeat(ROOM_ID_LENGTH + 1),
            // 长度对但混进了 base64url 之外的字符
            "r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97*",
        ] {
            let failure = build_request("cat.example.com", broken, "token", "device-1")
                .expect_err("非法 Room 必须被拒绝");

            assert!(failure.fatal, "非法 Room 应当是 fatal：{broken:?}");
        }
    }
}

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

use super::protocol::PROTOCOL_VERSION;

pub type PairSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 把用户填的地址补全成 `/ws` 升级地址。
///
/// 用户填的通常是部署输出里的 `https://<worker>.workers.dev`，但 WebSocket 客户端
/// 只接受 `ws` / `wss` scheme（`http://` 会直接报 URL scheme not supported），
/// 所以这里必须把 scheme 换掉。没写 scheme 时按 https 处理。
pub fn build_upgrade_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_end_matches('/');

    if trimmed.is_empty() {
        return Err("Relay URL 不能为空".into());
    }

    let with_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    };

    if with_scheme.ends_with("/ws") {
        Ok(with_scheme)
    } else {
        Ok(format!("{with_scheme}/ws"))
    }
}

pub async fn connect(
    relay_url: &str,
    auth_token: &str,
    device_id: &str,
) -> Result<PairSocket, String> {
    let url = build_upgrade_url(relay_url)?;

    let mut request = url
        .into_client_request()
        .map_err(|err| format!("Relay URL 不合法: {err}"))?;

    {
        let headers = request.headers_mut();
        let authorization = HeaderValue::from_str(&format!("Bearer {auth_token}"))
            .map_err(|_| "Pair Secret 含非法字符".to_string())?;
        let client = HeaderValue::from_str(device_id)
            .map_err(|_| "deviceId 含非法字符".to_string())?;
        let protocol = HeaderValue::from_str(&PROTOCOL_VERSION.to_string())
            .map_err(|_| "协议版本不合法".to_string())?;

        headers.insert("authorization", authorization);
        headers.insert("x-bongo-client", client);
        headers.insert("x-bongo-protocol", protocol);
    }

    let (socket, _response) = connect_async(request)
        .await
        .map_err(describe_connect_error)?;

    Ok(socket)
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

fn describe_connect_error(error: WsError) -> String {
    match error {
        WsError::Http(response) => {
            let status = response.status();

            match status.as_u16() {
                401 => "鉴权失败：Pair Secret 与部署时的值不一致".to_string(),
                426 => "协议版本不匹配：中继需要 protocol 1".to_string(),
                400 => "deviceId 被中继拒绝".to_string(),
                404 => "Relay URL 路径不对：应该指向 /ws".to_string(),
                _ => format!("中继返回 HTTP {}", status.as_u16()),
            }
        }
        WsError::Url(err) => format!("Relay URL 不合法: {err}"),
        WsError::Io(err) => format!("连接失败: {err}"),
        other => format!("连接失败: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! 极小的 HTTP/1.1 工具：读请求头、写响应、写 WebSocket 升级响应。
//!
//! 为什么要自己读请求头（而不是交给 `tokio_tungstenite::accept_hdr_async`）：
//! 中继要先用普通 HTTP 回答 `/health`、404、401、426、400，再决定是否升级。
//! 自己读还能保证**不会多读**——握手成功的连接紧接着就是 WebSocket 帧，
//! 多读一个字节都会丢掉它们，所以这里逐字节读到 `\r\n\r\n` 为止。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 请求头上限：正常握手只有几百字节，超过就是有人在乱发
pub const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    headers: Vec<(String, String)>,
}

impl RequestHead {
    /// 头名大小写不敏感（HTTP 头本身就不区分大小写）
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();

        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// 去掉 query / fragment 的路径
    pub fn path(&self) -> &str {
        self.target.split(['?', '#']).next().unwrap_or_default()
    }
}

pub async fn read_request_head<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<RequestHead, String> {
    let mut buffer: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];

    loop {
        if let Some(end) = head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..end]);

            return parse_head(&head);
        }

        if buffer.len() >= MAX_REQUEST_HEAD_BYTES {
            return Err("请求头过大".into());
        }

        let read = reader
            .read(&mut byte)
            .await
            .map_err(|error| error.to_string())?;

        if read == 0 {
            return Err("连接在请求头结束前被关闭".into());
        }

        buffer.push(byte[0]);
    }
}

/// 返回请求头结束的位置（`\r\n\r\n` / `\n\n` 之前的第一个字节不算在内）
fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| buffer.windows(2).position(|window| window == b"\n\n"))
}

fn parse_head(head: &str) -> Result<RequestHead, String> {
    let mut lines = head.split('\n').map(|line| line.trim_end_matches('\r'));
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().trim().to_string();
    let target = parts.next().unwrap_or_default().trim().to_string();

    if method.is_empty() || target.is_empty() {
        return Err("请求行不合法".into());
    }

    let mut headers = Vec::new();

    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let Some((name, value)) = line.split_once(':') else {
            // 不要把原文写进错误串：漏了冒号的 `Authorization: Bearer …` 会把凭据
            // 一路带进 `server.rs` 的日志。只报行号与长度。
            return Err(format!(
                "第 {} 行请求头缺少冒号（{} 字节）",
                // 请求行已经被 `next()` 取走，所以这里的第一行是第 2 行
                index + 2,
                line.len()
            ));
        };

        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

pub async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write_response_with_headers(writer, status, reason, content_type, "", body).await
}

/// 同上，但可以多带几行响应头（每行以 `\r\n` 结尾）。
///
/// RFC 6455 §4.4 要求「版本不支持」的 426 响应带上 `Sec-WebSocket-Version`。
/// `extra_headers` 的每一行都必须自带结尾的 `\r\n`（最后一行也要），否则拼出来的
/// 响应会把 body 当成响应头。
pub async fn write_response_with_headers<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    extra_headers: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
        body.len()
    );

    writer.write_all(response.as_bytes()).await?;
    writer.flush().await
}

pub async fn write_upgrade<W: AsyncWrite + Unpin>(
    writer: &mut W,
    accept_key: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept_key}\r\n\r\n"
    );

    writer.write_all(response.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_websocket_handshake() {
        let head = "GET /ws HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer abc\r\nX-Bongo-Client: 0F8F\r\n\r\n";
        let parsed = parse_head(head).unwrap();

        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.target, "/ws");
        assert_eq!(parsed.path(), "/ws");
        assert_eq!(parsed.header("authorization"), Some("Bearer abc"));
        assert_eq!(parsed.header("X-BONGO-CLIENT"), Some("0F8F"));
        assert_eq!(parsed.header("missing"), None);
    }

    #[test]
    fn strips_query_and_fragment_from_the_path() {
        let parsed = parse_head("GET /ws?x=1 HTTP/1.1\r\n\r\n").unwrap();

        assert_eq!(parsed.path(), "/ws");
    }

    #[test]
    fn tolerates_lf_only_line_endings() {
        let parsed = parse_head("GET /health HTTP/1.1\nHost: localhost\n\n").unwrap();

        assert_eq!(parsed.path(), "/health");
        assert_eq!(parsed.header("host"), Some("localhost"));
    }

    #[test]
    fn rejects_a_broken_request_line_or_header() {
        assert!(parse_head("\r\n\r\n").is_err());
        assert!(parse_head("GET\r\n\r\n").is_err());
        assert!(parse_head("GET /ws HTTP/1.1\r\nbroken header\r\n\r\n").is_err());
    }

    #[test]
    fn a_broken_header_line_never_echoes_its_content() {
        // 漏了冒号的 Authorization 头不能把凭据带进错误串（`server.rs` 会记日志）
        let error =
            parse_head("GET /ws HTTP/1.1\r\nAuthorization Bearer s3cret\r\n\r\n").unwrap_err();

        assert_eq!(error, "第 2 行请求头缺少冒号（27 字节）");
        assert!(!error.contains("s3cret"));
    }

    #[test]
    fn finds_both_line_ending_styles() {
        assert_eq!(head_end(b"GET /ws HTTP/1.1\r\n\r\nrest"), Some(16));
        assert_eq!(head_end(b"GET /ws HTTP/1.1\n\nrest"), Some(16));
        assert_eq!(head_end(b"GET /ws HTTP/1.1\r\n"), None);
    }
}

//! 内置的最小 STUN 服务（RFC 5389 的 Binding 请求 / 成功响应）。
//!
//! 为什么要它：WebRTC 打洞前，每台电脑得先知道自己的**公网**地址，而那只能问一台
//! 公网上的服务器。没有它，两台电脑手里都只有局域网地址（`192.168.x.x`），隔着两个
//! 家庭网络永远连不上——P2P 在「默认部署」下就等于没有。
//!
//! 以前的做法是另装 coturn、再手填 `PAIR_ICE_SERVERS`，部署者几乎都会漏掉。现在中继
//! 自己在一个 UDP 端口上回答「你的公网地址是多少」，并在 `server.welcome` 里把这个
//! 地址广告给**通过了服务器密码**的客户端（见 `Relay::ice_servers_for`）。
//!
//! 只做一件事：收到合法的 Binding 请求，就回一个带 `XOR-MAPPED-ADDRESS` 的成功响应。
//! 不做 TURN（不转发任何数据）、不做鉴权、不记日志（公网扫描器会把它刷满），也不保
//! 存任何状态。响应只有 32 字节（IPv4），和请求差不多大，放大系数很小。

use std::net::{IpAddr, SocketAddr};

use tokio::net::UdpSocket;

/// STUN 的魔数（RFC 5389 §6）
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// 消息头 20 字节：类型 2 + 长度 2 + 魔数 4 + 事务 ID 12
const HEADER_SIZE: usize = 20;
/// Binding 请求 / 成功响应
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
/// `XOR-MAPPED-ADDRESS` 属性
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// 比这大的包不可能是我们要回答的 Binding 请求，直接丢（也顺便限住读缓冲）
const MAX_REQUEST_SIZE: usize = 548;

/// 内置 STUN 的默认 UDP 端口。
///
/// 故意**不用** 3478：那是 coturn 的默认端口，部署者按 README 装了 coturn 时两个会抢
/// 同一个端口。注意 coturn 4.6 及更早的版本默认开着 RFC 5780，会**顺带**占用
/// `listening-port + 1` = 3479；4.7 起默认关掉了（仓库示例用的 `coturn/coturn:latest`
/// 没这个问题）。撞上时换一个 `PAIR_STUN_PORT` 即可。
pub const DEFAULT_STUN_PORT: u16 = 3479;

/// 这个包是不是一个我们该回答的 Binding 请求。是的话返回它的事务 ID。
///
/// 检查的是 RFC 5389 规定的全部头部不变量：前两位为 0、类型是 Binding 请求、魔数
/// 对得上、长度是 4 的倍数且与包长一致。任何一条不满足都不回——不回是最安全的失败。
pub fn parse_binding_request(packet: &[u8]) -> Option<[u8; 12]> {
    if packet.len() < HEADER_SIZE || packet.len() > MAX_REQUEST_SIZE {
        return None;
    }

    let message_type = u16::from_be_bytes([packet[0], packet[1]]);
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let cookie = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);

    if message_type != BINDING_REQUEST
        || cookie != MAGIC_COOKIE
        || length % 4 != 0
        || HEADER_SIZE + length != packet.len()
    {
        return None;
    }

    let mut transaction_id = [0u8; 12];

    transaction_id.copy_from_slice(&packet[8..HEADER_SIZE]);

    Some(transaction_id)
}

/// 组一个 Binding 成功响应，里面只有一个 `XOR-MAPPED-ADDRESS`（RFC 5389 §15.2）。
pub fn binding_success(transaction_id: [u8; 12], mapped: SocketAddr) -> Vec<u8> {
    let port = mapped.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut value = vec![0u8, 0u8];

    value.extend_from_slice(&port.to_be_bytes());

    // IPv4-mapped IPv6（`::ffff:a.b.c.d`）按 IPv4 报：双栈 socket 上的 IPv4 客户端
    // 看到的就是这种形状，报成 IPv6 对方会用不了
    let ip = match mapped.ip() {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };

    match ip {
        IpAddr::V4(v4) => {
            value[0..2].copy_from_slice(&[0x00, 0x01]);

            let xored = u32::from(v4) ^ MAGIC_COOKIE;

            value.extend_from_slice(&xored.to_be_bytes());
        }
        IpAddr::V6(v6) => {
            value[0..2].copy_from_slice(&[0x00, 0x02]);

            // IPv6 用「魔数 + 事务 ID」这 16 字节做异或
            let mut key = [0u8; 16];

            key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            key[4..].copy_from_slice(&transaction_id);

            for (byte, mask) in v6.octets().iter().zip(key) {
                value.push(byte ^ mask);
            }
        }
    }

    let attribute_length = value.len() as u16;
    let body_length = 4 + value.len() as u16;
    let mut response = Vec::with_capacity(HEADER_SIZE + usize::from(body_length));

    response.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    response.extend_from_slice(&body_length.to_be_bytes());
    response.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    response.extend_from_slice(&transaction_id);
    response.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    response.extend_from_slice(&attribute_length.to_be_bytes());
    response.extend_from_slice(&value);

    response
}

/// 在 `socket` 上一直回答 Binding 请求，直到 socket 出错。
///
/// 单个坏包、单次发送失败都只丢那一个包：UDP 上没有「连接」可断，没理由停服务。
pub async fn serve(socket: UdpSocket) {
    let mut buffer = [0u8; MAX_REQUEST_SIZE + 1];

    loop {
        let (size, from) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            // Windows 上对端端口不可达会让下一次 recv 报 ConnectionReset；Linux 上
            // 也可能有瞬时错误。都不致命，继续收
            Err(_) => continue,
        };

        if let Some(transaction_id) = parse_binding_request(&buffer[..size]) {
            let _ = socket
                .send_to(&binding_success(transaction_id, from), from)
                .await;
        }
    }
}

/// 从 HTTP `Host` 头里取出主机名（去掉端口），用来拼广告给客户端的 `stun:` 地址。
///
/// 客户端连中继用的是哪个地址，它的 STUN 请求就发到哪个地址——同一台机器，同一个
/// 名字，部署者什么都不用填。`Host` 是客户端自己给的，这里只做形状校验：它最多只能
/// 影响**它自己**收到的那份 welcome。
pub fn host_without_port(host: &str) -> Option<&str> {
    let host = host.trim();

    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量：`[::1]` 或 `[::1]:8080`，方括号要留着（`stun:[::1]:3479`）
        let end = rest.find(']')?;
        let after = &rest[end + 1..];

        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }

        let inner = &rest[..end];

        if inner.is_empty()
            || !inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return None;
        }

        &host[..end + 2]
    } else {
        let name = match host.split_once(':') {
            Some((name, port)) if is_port(port) => name,
            Some(_) => return None,
            None => host,
        };

        if name.is_empty()
            || name.len() > 253
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return None;
        }

        name
    };

    Some(name)
}

fn is_port(text: &str) -> bool {
    !text.is_empty() && text.len() <= 5 && text.parse::<u16>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(transaction_id: [u8; 12], attributes: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();

        packet.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        packet.extend_from_slice(&(attributes.len() as u16).to_be_bytes());
        packet.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        packet.extend_from_slice(&transaction_id);
        packet.extend_from_slice(attributes);

        packet
    }

    #[test]
    fn accepts_a_plain_binding_request_and_one_with_attributes() {
        let id = [7u8; 12];

        assert_eq!(parse_binding_request(&request(id, &[])), Some(id));
        // FINGERPRINT 这类属性客户端可能会带，长度对得上就收
        assert_eq!(
            parse_binding_request(&request(id, &[0x80, 0x28, 0, 4, 1, 2, 3, 4])),
            Some(id)
        );
    }

    #[test]
    fn rejects_anything_that_is_not_a_well_formed_binding_request() {
        let id = [1u8; 12];
        let good = request(id, &[]);

        // 太短
        assert_eq!(parse_binding_request(&good[..19]), None);
        // 类型不对（这是一个成功响应，别拿响应去回响应）
        let mut response = good.clone();
        response[0..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
        assert_eq!(parse_binding_request(&response), None);
        // 魔数不对（RFC 3489 的老客户端 / 随便什么 UDP 包）
        let mut cookie = good.clone();
        cookie[4] ^= 0xFF;
        assert_eq!(parse_binding_request(&cookie), None);
        // 长度与包长不一致
        let mut length = good.clone();
        length[3] = 4;
        assert_eq!(parse_binding_request(&length), None);
        // 长度不是 4 的倍数
        assert_eq!(parse_binding_request(&request(id, &[0, 0])), None);
        // 太大
        assert_eq!(parse_binding_request(&request(id, &[0u8; 600])), None);
    }

    /// RFC 5769 §2.2 的固定向量：IPv4 的 XOR-MAPPED-ADDRESS 编码
    #[test]
    fn encodes_the_rfc_5769_ipv4_vector() {
        let id = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let response = binding_success(id, "192.0.2.1:32853".parse().unwrap());

        assert_eq!(&response[0..2], &[0x01, 0x01]);
        assert_eq!(&response[2..4], &[0x00, 0x0c]);
        assert_eq!(&response[8..20], &id);
        assert_eq!(
            &response[20..],
            &[0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]
        );
    }

    /// RFC 5769 §2.3 的固定向量：IPv6 的 XOR-MAPPED-ADDRESS 编码
    #[test]
    fn encodes_the_rfc_5769_ipv6_vector() {
        let id = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let response = binding_success(
            id,
            "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                .parse()
                .unwrap(),
        );

        assert_eq!(&response[2..4], &[0x00, 0x18]);
        assert_eq!(
            &response[20..],
            &[
                0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3,
                0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
            ]
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_is_reported_as_ipv4() {
        let id = [3u8; 12];
        let mapped = binding_success(id, "[::ffff:192.0.2.1]:32853".parse().unwrap());
        let plain = binding_success(id, "192.0.2.1:32853".parse().unwrap());

        assert_eq!(mapped, plain);
    }

    #[test]
    fn host_header_is_reduced_to_a_bare_name() {
        assert_eq!(
            host_without_port("cat.example.com"),
            Some("cat.example.com")
        );
        assert_eq!(
            host_without_port("cat.example.com:443"),
            Some("cat.example.com")
        );
        assert_eq!(
            host_without_port("47.109.69.191:8080"),
            Some("47.109.69.191")
        );
        assert_eq!(host_without_port("[::1]:8080"), Some("[::1]"));
        assert_eq!(host_without_port("[2001:db8::1]"), Some("[2001:db8::1]"));
        assert_eq!(host_without_port(" localhost "), Some("localhost"));

        assert_eq!(host_without_port(""), None);
        assert_eq!(host_without_port("cat.example.com:abc"), None);
        assert_eq!(host_without_port("a:b:c"), None);
        assert_eq!(host_without_port("evil\"host"), None);
        assert_eq!(host_without_port("[::1]x"), None);
        assert_eq!(host_without_port("[]"), None);
    }

    /// 真的走一遍 UDP：发一个 Binding 请求，回来的地址就是我们自己的源地址
    #[tokio::test]
    async fn answers_over_real_udp_with_the_source_address() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();

        tokio::spawn(serve(server));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_address = client.local_addr().unwrap();
        let id = [9u8; 12];

        // 先发一个垃圾包：不该有回应，也不该让服务停掉
        client.send_to(b"hello", server_address).await.unwrap();
        client
            .send_to(&request(id, &[]), server_address)
            .await
            .unwrap();

        let mut buffer = [0u8; 64];
        let (size, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            &buffer[..size],
            binding_success(id, client_address).as_slice()
        );
    }
}

//! 自建中继入口：读配置、监听、把连接交给 `server`。

use bongocat_pair_relay::relay::Relay;
use bongocat_pair_relay::{server, stun};
use tokio::net::{TcpListener, UdpSocket};

#[tokio::main]
async fn main() {
    // 容器健康检查（`docker-compose.yml` 的 healthcheck）走这条极简路径：
    // 探一次本机 `/health`，看得见 JSON 就退出 0。它不需要额外依赖，也不会碰
    // 会话层——`docker compose ps` 里显示的「healthy」就是「中继真的在服务」。
    if std::env::args().any(|arg| arg == "--health-check") {
        std::process::exit(match health_check().await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("健康检查失败：{error}");

                1
            }
        });
    }

    if let Err(error) = run().await {
        eprintln!("中继启动失败：{error}");
        std::process::exit(1);
    }
}

/// `GET /health` 一次，确认它返回 200 且带 `"ok":true`
async fn health_check() -> Result<(), String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listen = server::listen_address().await?;
    let target = health_target(&listen);
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::net::TcpStream::connect(&target),
    )
    .await
    .map_err(|_| format!("连接 {target} 超时"))?
    .map_err(|error| format!("连接 {target} 失败: {error}"))?;

    stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .map_err(|error| format!("向 {target} 发送 /health 失败: {error}"))?;

    let mut response = Vec::new();

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        stream.read_to_end(&mut response),
    )
    .await
    .map_err(|_| format!("读取 {target} 的 /health 响应超时"))?
    .map_err(|error| format!("读取 {target} 的 /health 响应失败: {error}"))?;

    let text = String::from_utf8_lossy(&response);

    if text.contains(" 200 ") && text.contains("\"ok\":true") {
        Ok(())
    } else {
        Err(format!("{target} 的 /health 返回异常：{}", text.trim()))
    }
}

/// 健康检查该连哪个地址：沿用 `PAIR_LISTEN` 里的主机，通配地址才回落到回环。
///
/// 部署者如果把 `PAIR_LISTEN` 写成具体网卡地址（例如 `172.17.0.2:8080`），固定探
/// `127.0.0.1` 会连不上，容器就永远停在 `unhealthy`，而 `docker-compose.yml` 里的
/// Caddy 是 `depends_on: service_healthy`——一个写法上的小改动会让整站起不来。
///
/// 两个 `timeout` 各 1 秒：compose 那边的 `timeout: 3s` 得留出余量，否则失败时
/// 由 docker 先杀掉，日志里看不到这句原因。
fn health_target(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((host, port)) => (host, port),
        // 只有端口（例如 `8080`）时按回环处理
        None => ("", listen),
    };

    let host = match host.trim() {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };

    format!("{host}:{port}")
}

async fn run() -> Result<(), String> {
    let config = server::load_config()?;
    let listen = server::listen_address().await?;
    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|error| format!("无法监听 {listen}: {error}"))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let limits = config.limits;

    // 内置 STUN（见 `stun.rs`）。绑不上端口**不致命**：转发、聊天都不依赖它，只是 P2P
    // 会退回「只有内网地址」。所以只打一条醒目的提示，并且不广告一个不存在的服务。
    let stun_port = match config.stun_port {
        Some(port) => match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                tokio::spawn(stun::serve(socket));

                Some(port)
            }
            Err(error) => {
                eprintln!(
                    "内置 STUN 无法监听 UDP {port}：{error}。P2P 直连将只能在同一局域网内成功；\
                     多半是端口被占了（旧版 coturn 会顺带占 3479），换一个空闲的 \
                     PAIR_STUN_PORT 即可（compose 的端口映射会跟着变）"
                );

                None
            }
        },
        None => None,
    };

    let relay = Relay::new(
        limits,
        config.max_sessions,
        config.stale_after,
        config.ice_servers.clone(),
        stun_port,
        config.server_verifier,
    );

    println!("BongoCat 双人中继已启动（多会话）：http://{address}");
    println!("  /health   健康检查（不需要鉴权）");
    println!(
        "  /ws       WebSocket 升级（Authorization / X-Bongo-Server / X-Bongo-Room / \
         X-Bongo-Client / X-Bongo-Protocol: 1）"
    );
    println!(
        "  会话       最多 {} 组同时在线（每组两台设备）",
        config.max_sessions
    );
    println!(
        "  额度       {:.0} 帧/秒 · {:.0} chunk/秒 · {:.0} MiB/秒",
        limits.frames_per_second,
        limits.chunks_per_second,
        limits.bytes_per_second / (1024.0 * 1024.0)
    );
    println!("  TLS       由前置的 Caddy 终结，本进程只说 HTTP/WS");
    match (stun_port, config.ice_servers.is_some()) {
        (Some(port), _) => {
            println!("  STUN      内置，UDP {port}（安全组要放行 UDP {port}，P2P 直连才打得通）")
        }
        (None, true) => println!("  STUN      使用 PAIR_ICE_SERVERS 里的配置，内置 STUN 未启动"),
        (None, false) => println!("  STUN      未启用：P2P 直连只能在同一局域网内成功"),
    }
    println!("  服务器密码 PAIR_SERVER_PASSWORD 已生效（摘要形式，进程里没有原文）");
    println!("            客户端「服务器密码」必须填同一个值，否则连握手都过不去");
    println!("  配对密码  本进程**没有**任何配对密码：那是每一对用户自己的凭据");
    println!("  提醒      中继不保存聊天与文件，只做转发");

    server::serve(listener, relay).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通配地址探回环，具体地址跟着 `PAIR_LISTEN` 走（P2：固定探回环会让
    /// 「绑具体网卡」的部署永远 `unhealthy`）
    #[test]
    fn the_health_target_follows_the_configured_host() {
        assert_eq!(health_target("0.0.0.0:8080"), "127.0.0.1:8080");
        assert_eq!(health_target("[::]:8080"), "127.0.0.1:8080");
        assert_eq!(health_target("8080"), "127.0.0.1:8080");
        assert_eq!(health_target("172.17.0.2:8080"), "172.17.0.2:8080");
        assert_eq!(health_target("127.0.0.2:18799"), "127.0.0.2:18799");
        // IPv6 字面量按**最后一个**冒号切分，方括号要留着（`TcpStream::connect` 要这个形状）
        assert_eq!(health_target("[::1]:8080"), "[::1]:8080");
    }
}

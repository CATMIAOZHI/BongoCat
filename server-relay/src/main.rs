//! 自建中继入口：读配置、监听、把连接交给 `server`。

use std::sync::Arc;

use bongocat_pair_relay::relay::Relay;
use bongocat_pair_relay::server;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("中继启动失败：{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = Arc::new(server::load_config()?);
    let listen = server::listen_address().await?;
    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|error| format!("无法监听 {listen}: {error}"))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let limits = config.limits;
    let relay = Relay::new(limits, config.stale_after, config.ice_servers.clone());

    println!("BongoCat 双人中继已启动：http://{address}");
    println!("  /health   健康检查（不需要鉴权）");
    println!("  /ws       WebSocket 升级（Authorization / X-Bongo-Client / X-Bongo-Protocol: 1）");
    println!(
        "  额度       {:.0} 帧/秒 · {:.0} chunk/秒 · {:.0} MiB/秒",
        limits.frames_per_second,
        limits.chunks_per_second,
        limits.bytes_per_second / (1024.0 * 1024.0)
    );
    println!("  TLS       由前置的 Caddy 终结，本进程只说 HTTP/WS");
    println!("  提醒      中继不保存聊天与文件，只做转发");

    server::serve(listener, config, relay).await;

    Ok(())
}

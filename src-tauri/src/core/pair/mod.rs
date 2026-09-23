//! 双人联机（Pair）功能。
//!
//! 网络连接只在 Rust 侧存在一份（[`manager::PairManager`]），所有 WebView 通过
//! Tauri 事件观察状态；Pair Secret 只进系统凭据库。

pub mod client;
pub mod crypto;
pub mod manager;
pub mod protocol;
pub mod secret;

#[cfg(test)]
mod e2e;

use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Manager as _, Runtime, State, command};

use manager::{AppEventSink, PairManager, PairStatus};
use protocol::{FrameKind, PresencePayload, PresenceState, message_type};

/// 应用启动时调用：准备设备 id，并把 PairManager 注册为全局状态
pub fn setup<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let device_id = manager::load_or_create_device_id(app)?;
    let sink = Arc::new(AppEventSink::new(app.clone()));

    app.manage(Arc::new(PairManager::new(device_id, sink)));

    Ok(())
}

#[command]
pub async fn pair_get_status(manager: State<'_, Arc<PairManager>>) -> Result<PairStatus, String> {
    Ok(manager.status())
}

#[command]
pub async fn pair_get_device_id(manager: State<'_, Arc<PairManager>>) -> Result<String, String> {
    Ok(manager.device_id())
}

#[command]
pub async fn pair_set_secret(secret: String) -> Result<(), String> {
    // 先校验格式再落盘，避免把打错的值写进凭据库
    let bytes = crypto::decode_pair_secret(&secret)?;
    let canonical =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);

    secret::set_secret(&canonical)
}

#[command]
pub async fn pair_has_secret() -> Result<bool, String> {
    secret::has_secret()
}

#[command]
pub async fn pair_delete_secret() -> Result<(), String> {
    secret::delete_secret()
}

#[command]
pub async fn pair_connect(
    manager: State<'_, Arc<PairManager>>,
    relay_url: String,
) -> Result<(), String> {
    Arc::clone(&manager).start(&relay_url, None)
}

#[command]
pub async fn pair_disconnect(manager: State<'_, Arc<PairManager>>) -> Result<(), String> {
    Arc::clone(&manager).disconnect();

    Ok(())
}

/// 测试用往返消息：对端收到后会回一条 `pair.pong`
#[command]
pub async fn pair_send_ping(manager: State<'_, Arc<PairManager>>) -> Result<(), String> {
    manager.send(
        FrameKind::Ping,
        message_type::PING,
        json!({ "sentAt": protocol::now_millis() }),
    )
}

#[command]
pub async fn pair_send_presence(
    manager: State<'_, Arc<PairManager>>,
    presence: PresenceState,
    message: Option<String>,
    display_name: Option<String>,
) -> Result<(), String> {
    let payload = serde_json::to_value(PresencePayload {
        state: presence,
        message,
        display_name,
    })
    .map_err(|err| format!("序列化 presence 失败: {err}"))?;

    manager.send(FrameKind::Presence, message_type::PRESENCE, payload)
}

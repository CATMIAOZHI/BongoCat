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
use protocol::{
    FrameKind, InputStats, PetSnapshot, PresencePayload, PresenceState, message_type,
};

/// 应用启动时调用：准备设备 id，并把 PairManager 注册为全局状态
///
/// 设备 id 落盘失败时退化为本次运行内的临时 id：这样 `manage` 一定成功，
/// 所有 pair 命令都可用（否则只有 3 个 secret 命令能工作，其余全部报
/// 「state not managed」，而原因只写在日志里，用户完全看不懂）。
pub fn setup<R: Runtime>(app: &AppHandle<R>) {
    let device_id = manager::load_or_create_device_id(app).unwrap_or_else(|error| {
        tauri_plugin_log::log::error!("设备 id 初始化失败，本次运行使用临时 id: {error}");

        uuid::Uuid::new_v4().to_string()
    });
    let sink = Arc::new(AppEventSink::new(app.clone()));

    app.manage(Arc::new(PairManager::new(device_id, sink)));
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
pub async fn pair_set_secret(secret: String) -> Result<String, String> {
    // 先校验格式再落盘，避免把打错的值写进凭据库
    let bytes = crypto::decode_pair_secret(&secret)?;
    let canonical =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);

    secret::set_secret(&canonical)?;

    // 只回显指纹（R17），不回显 secret 本身：双方可以靠它核对填的是不是同一个值
    Ok(crypto::secret_fingerprint(&bytes))
}

#[command]
pub async fn pair_has_secret() -> Result<bool, String> {
    secret::has_secret()
}

/// 重新读出已保存 secret 的指纹（R17）。
///
/// `pair_set_secret` 只在写入时回显一次指纹，前端不落盘；没有这个命令，重启后
/// 偏好页就不知道有没有存过 secret，「已配置」标记、核对指纹和删除入口都不会出现。
#[command]
pub async fn pair_get_secret_fingerprint() -> Result<Option<String>, String> {
    let Some(raw) = secret::load_secret()? else {
        return Ok(None);
    };

    let bytes = crypto::decode_pair_secret(&raw)?;

    Ok(Some(crypto::secret_fingerprint(&bytes)))
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

/// 实时宠物快照：可覆盖发送（拥塞时 latest wins），与 R4 的「量化后变化才发」配合
#[command]
pub async fn pair_send_pet_state(
    manager: State<'_, Arc<PairManager>>,
    snapshot: PetSnapshot,
) -> Result<(), String> {
    let payload = serde_json::to_value(snapshot.sanitized())
        .map_err(|err| format!("序列化宠物快照失败: {err}"))?;

    manager.send_replaceable(FrameKind::PetState, message_type::PET_STATE, payload)
}

/// 输入统计：同样走可覆盖通道（§84），由前端按 30 秒 / 建连时一次的节奏调用
#[command]
pub async fn pair_send_stats(
    manager: State<'_, Arc<PairManager>>,
    stats: InputStats,
) -> Result<(), String> {
    let payload =
        serde_json::to_value(stats).map_err(|err| format!("序列化输入统计失败: {err}"))?;

    manager.send_replaceable(FrameKind::Stats, message_type::STATS, payload)
}

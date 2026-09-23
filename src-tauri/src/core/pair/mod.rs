//! 双人联机（Pair）功能。
//!
//! 网络连接只在 Rust 侧存在一份（[`manager::PairManager`]），所有 WebView 通过
//! Tauri 事件观察状态；Pair Secret 只进系统凭据库。

pub mod client;
pub mod crypto;
pub mod history;
pub mod manager;
pub mod protocol;
pub mod secret;

#[cfg(test)]
mod e2e;

use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Manager as _, Runtime, State, command};

use history::{ChatMessage, ExportFormat, ExportSummary, HistoryPage, HistoryStats, PairHistory};
use manager::{AppEventSink, PairManager, PairStatus};
use protocol::{FrameKind, InputStats, PetSnapshot, PresencePayload, PresenceState, message_type};

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
    let history = Arc::new(open_history(app));
    let sink = Arc::new(AppEventSink::new(app.clone()));

    app.manage(Arc::new(PairManager::new(device_id, sink, history)));
}

/// 打开本地聊天库。
///
/// 打不开时退化成内存库：聊天记录不落盘，但连接、宠物同步这些功能必须照常可用，
/// 否则一个磁盘问题会让整个联机功能失效。
fn open_history<R: Runtime>(app: &AppHandle<R>) -> PairHistory {
    let path = app
        .path()
        .app_config_dir()
        .ok()
        .map(|directory| directory.join("pair").join("pair.db"));

    match path {
        Some(path) => PairHistory::open(&path).unwrap_or_else(fallback_history),
        None => fallback_history("无法定位聊天数据库路径".to_string()),
    }
}

fn fallback_history(reason: String) -> PairHistory {
    tauri_plugin_log::log::error!("聊天数据库不可用（本次运行不落盘）: {reason}");

    PairHistory::in_memory().unwrap_or_else(|error| panic!("内存聊天数据库不可用: {error}"))
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
    // §33：本地统计同时在 SQLite 里留一份（按天一行），供以后回看
    if !stats.date.is_empty()
        && let Err(error) = manager.history().upsert_input_stats(
            &stats.date,
            stats.today_keyboard,
            stats.today_mouse,
        )
    {
        tauri_plugin_log::log::warn!("写入输入统计失败: {error}");
    }

    let payload =
        serde_json::to_value(stats).map_err(|err| format!("序列化输入统计失败: {err}"))?;

    manager.send_replaceable(FrameKind::Stats, message_type::STATS, payload)
}

/// 发一条文本消息（§31）。
///
/// 返回本地已经落库的那一行：连不上时状态是 `pending`，会在对端上线后自动重发（§32）。
#[command]
pub async fn pair_send_chat(
    manager: State<'_, Arc<PairManager>>,
    text: String,
) -> Result<ChatMessage, String> {
    Arc::clone(&manager).send_chat(&text)
}

/// 读取聊天历史（§34），每次最多一页，滚到顶部再往前翻
#[command]
pub async fn pair_history_list(
    manager: State<'_, Arc<PairManager>>,
    before: Option<i64>,
    limit: Option<usize>,
) -> Result<HistoryPage, String> {
    let history = manager.history();
    let limit = limit
        .unwrap_or(history::DEFAULT_PAGE_LIMIT)
        .clamp(1, history::MAX_PAGE_LIMIT);

    history.list(history.epoch()?, before, limit)
}

/// 本地保存进度（§36）：当前周期条数 + 总条数 + 当前周期号
#[command]
pub async fn pair_history_stats(
    manager: State<'_, Arc<PairManager>>,
) -> Result<HistoryStats, String> {
    let history = manager.history();
    let epoch = history.epoch()?;

    Ok(HistoryStats {
        epoch,
        current: history.count(Some(epoch))?,
        total: history.count(None)?,
    })
}

/// 导出聊天记录到用户选定的路径（§35）。附件不进 JSON，Phase 5 会导出到同目录。
#[command]
pub async fn pair_history_export(
    manager: State<'_, Arc<PairManager>>,
    format: ExportFormat,
    path: String,
) -> Result<ExportSummary, String> {
    let messages = manager.history().all()?;
    let exported_at = protocol::now_millis();
    let content = format.render(&messages, exported_at)?;

    std::fs::write(&path, content).map_err(|err| format!("写入导出文件失败: {err}"))?;

    Ok(ExportSummary {
        path,
        format,
        messages: messages.len(),
        exported_at,
    })
}

/// 导出并开始新的记录周期（§36）。`deleteOld` 为真时删除旧周期消息，默认保留。
#[command]
pub async fn pair_history_start_new_epoch(
    manager: State<'_, Arc<PairManager>>,
    delete_old: Option<bool>,
) -> Result<i64, String> {
    manager
        .history()
        .start_new_epoch(delete_old.unwrap_or(false))
}

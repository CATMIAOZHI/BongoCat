//! 双人联机（Pair）功能。
//!
//! 网络连接只在 Rust 侧存在一份（[`manager::PairManager`]），所有 WebView 通过
//! Tauri 事件观察状态；Pair Secret 只进系统凭据库。

pub mod audio;
pub mod client;
pub mod crypto;
pub mod history;
// P2P 传输的 cfg 中立门面（R24-1）：Windows 上转发到 `p2p`，其它平台是空实现，
// 这样 `live` 的调用点不必散写 cfg。
pub mod link;
pub mod manager;
// P2P（WebRTC）传输。只在 Windows 上编译：Phase 8 的范围就是 Windows 客户端，
// 非 Windows 的 release 目标连 webrtc 那棵依赖树都不编译（见 src-tauri/Cargo.toml）。
#[cfg(windows)]
pub mod p2p;
pub mod protocol;
pub mod secret;
pub mod transfer;

#[cfg(test)]
mod e2e;

use std::sync::{Arc, Mutex};

use rand::Rng as _;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Manager as _, Runtime, State, command};

use history::{ChatMessage, ExportFormat, ExportSummary, HistoryPage, HistoryStats, PairHistory};
use history::{MessageKind, NewAttachment, NewMessage};
use manager::{AppEventSink, OutgoingRequest, PairManager, PairStatus};
use protocol::{
    FrameKind, InputStats, PetSnapshot, PresencePayload, PresenceState, TransferKind, message_type,
};
use transfer::{TransferStore, sanitize_file_name, sanitize_mime, sha256_file};

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
    let store = match pair_root(app) {
        Some(root) => TransferStore::new(root),
        // 定位不到配置目录时退到系统临时目录：附件仍然可用，只是重启后会被清理
        None => TransferStore::new(std::env::temp_dir().join("bongocat-pair")),
    };

    if let Err(error) = store.ensure() {
        tauri_plugin_log::log::error!("附件目录不可用（附件功能会失败）: {error}");
    }

    // R41：上一次运行里「录完还没确认」的 wav 进程一退就没人认领了，启动时顺手清一次
    // （必须在运行前：运行中调它会把用户手上待确认的那条录音删掉）
    store.cleanup_orphan_recordings();

    app.manage(Arc::new(PairManager::new(device_id, sink, history, store)));
    app.manage(PairRecording::default());
}

/// pair 功能的落盘根目录：`<配置目录>/pair`（聊天库、附件、临时文件都在这里）
fn pair_root<R: Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|directory| directory.join("pair"))
}

/// 打开本地聊天库。
///
/// 打不开时退化成内存库：聊天记录不落盘，但连接、宠物同步这些功能必须照常可用，
/// 否则一个磁盘问题会让整个联机功能失效。
fn open_history<R: Runtime>(app: &AppHandle<R>) -> PairHistory {
    let path = pair_root(app).map(|directory| directory.join("pair.db"));

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

/// 生成一个新的配对密码（§22）。
///
/// 用系统的 CSPRNG 取 32 字节再编成 base64url，**不用** `Math.random`、时间戳或 UUID：
/// 这个值就是双方的 E2EE 密钥材料，可预测等于没有加密。生成结果只返回给这一次调用
/// （用户看得见、能复制），要留下就自己点「保存」——它不会被偷偷写进凭据库。
#[command]
pub async fn pair_generate_secret() -> Result<String, String> {
    let mut bytes = [0u8; crypto::PAIR_SECRET_BYTES];

    rand::rng().fill_bytes(&mut bytes);

    // 回显的形态与 `decode_pair_secret` 接受的一致（base64url 无填充）
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        bytes,
    ))
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

/// 保存「服务器密码」（R36）。
///
/// 它**不是**密钥材料：不参与 E2EE，也不参与「谁是同一对」的判断，只是「能不能用这台
/// 服务器」的门槛，所以它单独存在另一个凭据条目里——换服务器密码不会连带换掉配对密码
/// （那会换掉 E2EE 根密钥，等于让对方重新填一次）。
#[command]
pub async fn pair_set_server_password(password: String) -> Result<(), String> {
    let trimmed = password.trim();

    if trimmed.is_empty() {
        return Err("服务器密码不能为空".into());
    }

    secret::set_server_password(trimmed)
}

#[command]
pub async fn pair_has_server_password() -> Result<bool, String> {
    secret::has_server_password()
}

#[command]
pub async fn pair_delete_server_password() -> Result<(), String> {
    secret::delete_server_password()
}

/// 连接中继（R36）。
///
/// `secret` / `server_password` 是**这一次连接**要用的值，允许直接来自输入框（还没点过
/// 「保存」也照样能连）；对应参数为 `None` 时回落到凭据库里存着的那个。两者都为空时
/// 「配对密码」会给出明确的错误，而「服务器密码」只是不带那个头（官方的 Cloudflare
/// 中继不需要它）。
#[command]
pub async fn pair_connect(
    manager: State<'_, Arc<PairManager>>,
    relay_url: String,
    secret: Option<String>,
    server_password: Option<String>,
) -> Result<(), String> {
    Arc::clone(&manager).start(
        &relay_url,
        secret.as_deref(),
        server_password.as_deref(),
    )
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

/// 附件与临时文件的落盘位置（前端要往临时目录里写粘贴的图片）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferPaths {
    pub root: String,
    pub attachments: String,
    pub tmp: String,
    /// 单个附件上限（字节）
    pub max_size: u64,
}

/// 查附件目录与上限
#[command]
pub fn pair_transfer_paths(manager: State<'_, Arc<PairManager>>) -> TransferPaths {
    let store = manager.store();

    TransferPaths {
        root: store.root().to_string_lossy().to_string(),
        attachments: store.attachments_dir().to_string_lossy().to_string(),
        tmp: store.tmp_dir().to_string_lossy().to_string(),
        max_size: manager.max_attachment_size(),
    }
}

/// 设置单个附件的上限（MB）。返回夹紧之后的字节数。
#[command]
pub fn pair_set_max_attachment_mb(manager: State<'_, Arc<PairManager>>, mb: u64) -> u64 {
    manager.set_max_attachment_mb(mb)
}

/// 发送一个附件（§37 / §38）。
///
/// `stage` 为真时把源文件移进本机附件缓存（粘贴的图片、录音这类临时文件用它），
/// 为假时直接引用用户选的那个文件。返回本地已经落库的那条消息。
#[command]
pub async fn pair_send_attachment(
    manager: State<'_, Arc<PairManager>>,
    path: String,
    kind: TransferKind,
    mime: Option<String>,
    stage: Option<bool>,
) -> Result<ChatMessage, String> {
    let source = std::path::PathBuf::from(path.trim());

    if !source.is_file() {
        return Err("找不到这个文件".to_string());
    }

    send_attachment(
        Arc::clone(&manager),
        source,
        kind,
        mime,
        stage.unwrap_or(false),
    )
    .await
}

/// 附件发送的公共部分（§38）：算校验值、入库、交给传输管线。
///
/// 粘贴的图片（§37）与录好的语音（§44）都走这里；它们的源文件是临时文件，
/// `stage` 为真时先被收进附件缓存。**失败路径也要把临时文件删掉**：
/// `stage_copy` 是唯一会删源文件的地方，而超限、读不到、建不了会话都会在它之前
/// 或之后提前返回，不删就会在 `tmp/` 里攒下没人认领的孤儿文件
/// （`cleanup_stale_parts` 只认 `.part`）。
async fn send_attachment(
    manager: Arc<PairManager>,
    source: std::path::PathBuf,
    kind: TransferKind,
    mime: Option<String>,
    stage: bool,
) -> Result<ChatMessage, String> {
    let result = stage_attachment(Arc::clone(&manager), source.clone(), kind, mime, stage).await;

    if stage && result.is_err() {
        // 已经成功 `stage_copy` 过的不存在了，删不到是正常的
        let _ = std::fs::remove_file(&source);
    }

    result
}

/// 真正的发送准备工作：算校验值、入库、交给传输管线
async fn stage_attachment(
    manager: Arc<PairManager>,
    source: std::path::PathBuf,
    kind: TransferKind,
    mime: Option<String>,
    stage: bool,
) -> Result<ChatMessage, String> {
    let original_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment")
        .to_string();
    let name = sanitize_file_name(&original_name);
    let max_size = manager.max_attachment_size();
    let declared_size = std::fs::metadata(&source)
        .map_err(|error| format!("读取附件信息失败: {error}"))?
        .len();

    if declared_size > max_size {
        return Err(too_large(max_size));
    }

    let source = if stage {
        manager.store().stage_copy(&source, &name)?
    } else {
        source
    };

    // 计算 SHA-256 可能要把几百 MB 读一遍，放到阻塞线程池里
    let hash_source = source.clone();
    let (sha256, size) = tokio::task::spawn_blocking(move || sha256_file(&hash_source))
        .await
        .map_err(|error| format!("计算校验值失败: {error}"))??;

    if size > max_size {
        return Err(too_large(max_size));
    }

    let mime = sanitize_mime(mime.as_deref().unwrap_or_default());
    let kind_of_message = match kind {
        TransferKind::Image => MessageKind::Image,
        TransferKind::File => MessageKind::File,
        TransferKind::Voice => MessageKind::Voice,
    };
    let created_at = protocol::now_millis();
    let attachment_id = uuid::Uuid::new_v4().to_string();
    let message_id = uuid::Uuid::new_v4().to_string();

    manager.history().upsert_attachment(&NewAttachment {
        id: attachment_id.clone(),
        kind: kind_of_message,
        original_name: Some(name.clone()),
        mime: Some(mime.clone()),
        size: Some(size),
        sha256: Some(sha256.clone()),
        // 发出的附件在本地留一条路径：聊天记录里还能再打开它
        local_path: Some(source.to_string_lossy().to_string()),
        created_at,
    })?;

    let message = manager.history().insert(&NewMessage::outgoing_attachment(
        message_id.clone(),
        kind_of_message,
        attachment_id.clone(),
        created_at,
        manager.history().epoch()?,
    ))?;

    let request = OutgoingRequest {
        transfer_id: manager::new_transfer_id(),
        message_id: message_id.clone(),
        attachment_id,
        kind,
        name,
        mime,
        size,
        sha256,
        path: source,
    };

    if let Err(error) = manager.start_transfer(request) {
        // 没连着、或者同时进行的传输太多：这条消息不能停在「等待发送」等一个永远不会
        // 到来的重发——标成 failed，UI 才会给出「重试」（§43）
        manager.fail_attachment(&message_id, &error);

        return Err(error);
    }

    Ok(manager.history().find(&message.id)?.unwrap_or(message))
}

fn too_large(max_size: u64) -> String {
    format!("附件超过上限（{} MB）", max_size / (1024 * 1024))
}

/// 语音超过附件上限时的说明（§42 / §45）。
///
/// 提示「还能录几秒」比只说「太大了」有用；但专业声卡可能报出 384kHz 以上的采样率，
/// 这时 1 MB 连一秒都装不下，按秒取整会算出 0，所以那种情况换一句话。
fn too_large_for_voice(max_size: u64, secs: u64) -> String {
    let mb = max_size / (1024 * 1024);

    if secs == 0 {
        return format!("附件上限是 {mb} MB，这个上限录不了语音，请在设置里调大");
    }

    format!("附件上限是 {mb} MB，这个上限只能录约 {secs} 秒语音")
}

/// 接收方同意接收（§42 的大文件确认）
#[command]
pub async fn pair_transfer_accept(
    manager: State<'_, Arc<PairManager>>,
    message_id: String,
) -> Result<(), String> {
    manager.accept_transfer(&message_id)
}

/// 接收方拒绝接收
#[command]
pub async fn pair_transfer_reject(
    manager: State<'_, Arc<PairManager>>,
    message_id: String,
) -> Result<(), String> {
    manager.reject_transfer(&message_id)
}

/// 取消一次正在进行的传输
#[command]
pub async fn pair_transfer_cancel(
    manager: State<'_, Arc<PairManager>>,
    message_id: String,
) -> Result<(), String> {
    manager.cancel_transfer(&message_id)
}

/// 重发一条失败的附件（§43）。接收方的失败只能请对方重发。
#[command]
pub async fn pair_attachment_retry(
    manager: State<'_, Arc<PairManager>>,
    message_id: String,
) -> Result<(), String> {
    Arc::clone(&manager).retry_attachment(&message_id)
}

/// 录完但还没发出去的一条语音（R41 的二次确认）。
///
/// 它在临时目录里是一份 wav，前端用 asset protocol 先试听；用户点「发送」才真正走
/// 附件管线，点「取消」就直接删掉。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDraft {
    /// 临时目录里的 wav 绝对路径
    pub path: String,
    /// 实际录到的时长（毫秒）：由样本数算出来，不是前端那个滴答
    pub duration_ms: u64,
}

/// 语音录音状态（§44 / §45）。录音本身跑在专用线程里，这里只是一个句柄盒子；
/// R41 之后还多存一条「录完待确认」的草稿。
#[derive(Default)]
pub struct PairRecording {
    recorder: Arc<audio::Recorder>,
    draft: Mutex<Option<VoiceDraft>>,
}

impl PairRecording {
    /// 记下待发送的草稿：替换掉上一份（连同它的临时文件一起删掉，别攒孤儿 wav）
    fn hold_draft(&self, draft: VoiceDraft) {
        self.discard_draft();

        if let Ok(mut held) = self.draft.lock() {
            *held = Some(draft);
        }
    }

    /// 取走待发送的草稿（发送与取消都要先把它从状态里摘出来）
    fn take_draft(&self) -> Option<VoiceDraft> {
        self.draft.lock().ok().and_then(|mut held| held.take())
    }

    /// 丢掉草稿并删掉它的临时文件
    fn discard_draft(&self) {
        if let Some(draft) = self.take_draft() {
            let _ = std::fs::remove_file(&draft.path);
        }
    }
}

/// 开始录音（§45 的 Pressed），返回麦克风的原生采样率
#[command]
pub async fn pair_start_recording(recording: State<'_, PairRecording>) -> Result<u32, String> {
    let recorder = Arc::clone(&recording.recorder);

    // 打开麦克风要等设备真的开始录（最多 5 秒），别把异步运行时的工作线程占住
    let sample_rate = tokio::task::spawn_blocking(move || recorder.start())
        .await
        .map_err(|error| format!("开始录音失败: {error}"))??;

    // R41：重新开始录音时，上一次没发出去的草稿作废——但必须放在**麦克风真的开了之后**：
    // 放在前面的话，「重录时麦克风打不开」会把用户上一段还没确认的录音连文件一起弄丢。
    recording.discard_draft();

    Ok(sample_rate)
}

/// 结束录音，但**先不发送**（R41 的二次确认）。
///
/// 返回 `None` 表示这次不该发出去：没在录，或者只轻点了一下（< 300 ms）。
/// 录好的 wav 留在临时目录里等用户确认：点「发送」走 `pair_send_recording`，
/// 点「取消」走 `pair_cancel_recording`——两者都会把这份临时文件收干净。
#[command]
pub async fn pair_stop_recording(
    manager: State<'_, Arc<PairManager>>,
    recording: State<'_, PairRecording>,
) -> Result<Option<VoiceDraft>, String> {
    let Some(current) = recording.recorder.take() else {
        return Ok(None);
    };

    // 收尾最多等一个轮询周期，但 join 是阻塞调用，别占着异步运行时
    let audio = tokio::task::spawn_blocking(move || current.finish())
        .await
        .map_err(|error| format!("结束录音失败: {error}"))??;

    if audio.is_too_short() {
        return Ok(None);
    }

    if audio.truncated {
        tauri_plugin_log::log::info!("录音到 {} 秒上限，已自动截断", audio::MAX_RECORDING_SECS);
    }

    let manager = Arc::clone(&manager);

    // §42：先按算出来的长度判断上限，别等写完再发现太大——那会在 tmp/ 里留下
    // 一个没人认领的 wav（`cleanup_stale_parts` 只认 `.part`）
    let max_size = manager.max_attachment_size();
    let size = audio::wav_size(&audio);

    if size > max_size {
        let secs = audio::wav_limit_secs(max_size, audio.sample_rate);

        return Err(too_large_for_voice(max_size, secs));
    }

    // 先落到临时目录，再让 `stage_copy` 用 UUID 收进附件缓存（§41 / §42）
    let path = manager.store().tmp_dir().join(format!(
        "voice-{}.wav",
        history::file_stamp(protocol::now_millis())
    ));
    let target = path.clone();
    let duration_ms = audio.duration_ms();

    let written = tokio::task::spawn_blocking(move || audio::write_wav(&target, &audio))
        .await
        .map_err(|error| format!("保存录音失败: {error}"))?;

    if let Err(error) = written {
        // 写坏的文件不留：下一次录音会覆盖同一个名字（同一毫秒内不会重名）
        let _ = std::fs::remove_file(&path);

        return Err(error);
    }

    let draft = VoiceDraft {
        path: path.to_string_lossy().to_string(),
        duration_ms,
    };

    recording.hold_draft(draft.clone());

    Ok(Some(draft))
}

/// 发送「录完待确认」的那条语音（R41）。
///
/// 与 `pair_stop_recording` 分开，是为了让用户先试听再决定。`stage` 为真：wav 是临时文件，
/// 发送前先收进附件缓存（成功路径里它会被移走，失败路径由 `send_attachment` 删掉）。
#[command]
pub async fn pair_send_recording(
    manager: State<'_, Arc<PairManager>>,
    recording: State<'_, PairRecording>,
) -> Result<ChatMessage, String> {
    let Some(draft) = recording.take_draft() else {
        return Err("没有等待发送的录音".to_string());
    };

    send_attachment(
        Arc::clone(&manager),
        std::path::PathBuf::from(draft.path),
        TransferKind::Voice,
        Some("audio/wav".to_string()),
        true,
    )
    .await
}

/// 放弃这次录音（§45 的「可取消」）：不落盘、不发送。
///
/// R41 之后它管两种「取消」：正在录的那一段（丢掉麦克风数据），以及录完待确认的那一份
/// （删掉临时 wav）。前端两个取消按钮都调它，用户不用分辨自己在哪个阶段。
#[command]
pub async fn pair_cancel_recording(recording: State<'_, PairRecording>) -> Result<(), String> {
    recording.discard_draft();

    let Some(current) = recording.recorder.take() else {
        return Ok(());
    };

    // 取消不该因为麦克风出错而失败：结果直接丢掉
    let _ = tokio::task::spawn_blocking(move || current.finish()).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_limit_message_never_says_zero_seconds() {
        // 普通麦克风：1 MB 大约 10 秒
        assert!(too_large_for_voice(1024 * 1024, 10).contains("10 秒"));

        // 专业声卡的极端采样率下按秒取整会变 0，这时不能让人读到「能录 0 秒」
        let message = too_large_for_voice(1024 * 1024, 0);

        assert!(message.contains("录不了语音"), "{message}");
        assert!(!message.contains("0 秒"), "{message}");
    }
}

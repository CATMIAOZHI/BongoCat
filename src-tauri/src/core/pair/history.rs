//! 本地聊天数据库（SQLite）。
//!
//! 聊天记录不进 Pinia，也不进设置文件：读写全在这里（见 docs/pair-plan.md §33）。
//! 表一次建全（`messages` / `attachments` / `input_stats` / `metadata`），附件列先留着，
//! Phase 5 直接用同一个库，不需要再迁移。
//!
//! 这个模块也是**纯存储**：不知道网络、不知道 Tauri，方便直接跑单测。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// 单条文本上限（§31）：8 KiB
pub const MESSAGE_TEXT_LIMIT: usize = 8 * 1024;
/// 历史分页：默认一页多少条（§34）
pub const DEFAULT_PAGE_LIMIT: usize = 50;
/// 单页上限，前端一次翻不了更多
pub const MAX_PAGE_LIMIT: usize = 200;
/// 建库时的初始记录周期
pub const DEFAULT_EPOCH: i64 = 1;

/// SQLite 建表语句。
///
/// `seq` 是本地自增序号，只用来分页（对端看不到）；`id` 是对端也能看到的 message id。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    seq                INTEGER PRIMARY KEY AUTOINCREMENT,
    id                 TEXT    NOT NULL UNIQUE,
    direction          TEXT    NOT NULL,
    kind               TEXT    NOT NULL,
    created_at         INTEGER NOT NULL,
    text               TEXT,
    status             TEXT    NOT NULL,
    attachment_id      TEXT,
    conversation_epoch INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_epoch_seq ON messages (conversation_epoch, seq);
CREATE INDEX IF NOT EXISTS messages_status ON messages (status);

CREATE TABLE IF NOT EXISTS attachments (
    id            TEXT PRIMARY KEY,
    kind          TEXT    NOT NULL,
    original_name TEXT,
    mime          TEXT,
    size          INTEGER,
    sha256        TEXT,
    local_path    TEXT,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS input_stats (
    date              TEXT PRIMARY KEY,
    keyboard_count    INTEGER NOT NULL,
    mouse_click_count INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const EPOCH_KEY: &str = "conversation_epoch";

/// 消息方向。`Incoming` 是对方发来的，`Outgoing` 是本机发出的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageDirection {
    Incoming,
    Outgoing,
}

impl MessageDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "incoming" => Ok(Self::Incoming),
            "outgoing" => Ok(Self::Outgoing),
            other => Err(format!("未知的消息方向: {other}")),
        }
    }
}

/// 消息类型。Phase 4 只产生 `Text`，其余留给附件与语音。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Text,
    Image,
    File,
    Voice,
}

impl MessageKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::File => "file",
            Self::Voice => "voice",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "image" => Ok(Self::Image),
            "file" => Ok(Self::File),
            "voice" => Ok(Self::Voice),
            other => Err(format!("未知的消息类型: {other}")),
        }
    }
}

/// 发送状态（§31）。`Received` 是收到的消息在本地库里的状态，不参与发送流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Pending,
    Sent,
    Delivered,
    Failed,
    Received,
}

impl MessageStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Received => "received",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "sent" => Ok(Self::Sent),
            "delivered" => Ok(Self::Delivered),
            "failed" => Ok(Self::Failed),
            "received" => Ok(Self::Received),
            other => Err(format!("未知的消息状态: {other}")),
        }
    }
}

/// 一条聊天消息，前端直接拿它渲染
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// 本地自增序号（分页游标）
    pub seq: i64,
    pub id: String,
    pub direction: MessageDirection,
    pub kind: MessageKind,
    pub created_at: i64,
    pub text: Option<String>,
    pub status: MessageStatus,
    pub attachment_id: Option<String>,
    /// 附件消息带上附件记录（§37）：读历史时一并填好，前端不用为每条消息再查一次
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRecord>,
    pub conversation_epoch: i64,
}

/// 附件记录（§33）。`local_path` 只是本机的落盘位置，永远不会发给对方（§39 / §78）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRecord {
    pub id: String,
    pub kind: MessageKind,
    pub original_name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub local_path: Option<String>,
    pub created_at: i64,
}

/// 待写入的一条附件记录
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub id: String,
    pub kind: MessageKind,
    pub original_name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub local_path: Option<String>,
    pub created_at: i64,
}

/// 待写入的一条新消息（`seq` 由 SQLite 生成，所以不在这里）
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub id: String,
    pub direction: MessageDirection,
    pub kind: MessageKind,
    pub created_at: i64,
    pub text: Option<String>,
    pub status: MessageStatus,
    pub attachment_id: Option<String>,
    pub conversation_epoch: i64,
}

impl NewMessage {
    /// 本机发出的文本：先落 `pending`，发出去之后由调用方改成 `sent`
    pub fn outgoing_text(id: String, text: String, created_at: i64, epoch: i64) -> Self {
        Self {
            id,
            direction: MessageDirection::Outgoing,
            kind: MessageKind::Text,
            created_at,
            text: Some(text),
            status: MessageStatus::Pending,
            attachment_id: None,
            conversation_epoch: epoch,
        }
    }

    /// 对方发来的文本
    pub fn incoming_text(id: String, text: String, created_at: i64, epoch: i64) -> Self {
        Self {
            id,
            direction: MessageDirection::Incoming,
            kind: MessageKind::Text,
            created_at,
            text: Some(text),
            status: MessageStatus::Received,
            attachment_id: None,
            conversation_epoch: epoch,
        }
    }

    /// 本机发出的附件（§37）：对方的 `transfer.verified` 才是真正的「已送达」
    pub fn outgoing_attachment(
        id: String,
        kind: MessageKind,
        attachment_id: String,
        created_at: i64,
        epoch: i64,
    ) -> Self {
        Self {
            id,
            direction: MessageDirection::Outgoing,
            kind,
            created_at,
            text: None,
            status: MessageStatus::Pending,
            attachment_id: Some(attachment_id),
            conversation_epoch: epoch,
        }
    }

    /// 对方发来的附件：先落 `pending`，校验通过后由上层改成 `received`，失败则 `failed`
    pub fn incoming_attachment(
        id: String,
        kind: MessageKind,
        attachment_id: String,
        created_at: i64,
        epoch: i64,
    ) -> Self {
        Self {
            id,
            direction: MessageDirection::Incoming,
            kind,
            created_at,
            text: None,
            status: MessageStatus::Pending,
            attachment_id: Some(attachment_id),
            conversation_epoch: epoch,
        }
    }
}

/// 一页历史：`messages` 按时间升序（越旧越靠前），`hasMore` 表示前面还有更旧的
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub messages: Vec<ChatMessage>,
    pub has_more: bool,
    pub epoch: i64,
}

/// 本地保存上限的进度提示（§36）：偏好页拿它显示「即将达到上限」
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStats {
    pub epoch: i64,
    pub current: i64,
    pub total: i64,
}

/// 导出结果：告诉前端写到哪、写了多少
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub path: String,
    pub format: ExportFormat,
    pub messages: usize,
    pub exported_at: i64,
}

/// 导出格式（§35）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Json,
    Txt,
    Md,
}

impl ExportFormat {
    pub fn render(self, messages: &[ChatMessage], exported_at: i64) -> Result<String, String> {
        match self {
            Self::Json => render_json(messages, exported_at),
            Self::Txt => Ok(render_text(messages)),
            Self::Md => Ok(render_markdown(messages)),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonExport<'a> {
    format_version: u32,
    exported_at: i64,
    message_count: usize,
    messages: Vec<ExportedMessage<'a>>,
}

/// 导出用的消息。
///
/// 除了 `local_path` 之外与 `ChatMessage` 一致：本机落盘位置是隐私（§78），而导出的文件
/// 是拿去备份甚至分享的，带上它就等于把 `C:\Users\...` 一起交出去。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportedMessage<'a> {
    seq: i64,
    id: &'a str,
    direction: MessageDirection,
    kind: MessageKind,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    status: MessageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment: Option<ExportedAttachment<'a>>,
    conversation_epoch: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportedAttachment<'a> {
    id: &'a str,
    kind: MessageKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<&'a str>,
    created_at: i64,
}

impl<'a> From<&'a ChatMessage> for ExportedMessage<'a> {
    fn from(message: &'a ChatMessage) -> Self {
        Self {
            seq: message.seq,
            id: &message.id,
            direction: message.direction,
            kind: message.kind,
            created_at: message.created_at,
            text: message.text.as_deref(),
            status: message.status,
            attachment_id: message.attachment_id.as_deref(),
            attachment: message.attachment.as_ref().map(ExportedAttachment::from),
            conversation_epoch: message.conversation_epoch,
        }
    }
}

impl<'a> From<&'a AttachmentRecord> for ExportedAttachment<'a> {
    fn from(attachment: &'a AttachmentRecord) -> Self {
        Self {
            id: &attachment.id,
            kind: attachment.kind,
            original_name: attachment.original_name.as_deref(),
            mime: attachment.mime.as_deref(),
            size: attachment.size,
            sha256: attachment.sha256.as_deref(),
            created_at: attachment.created_at,
        }
    }
}

fn render_json(messages: &[ChatMessage], exported_at: i64) -> Result<String, String> {
    let document = JsonExport {
        format_version: 1,
        exported_at,
        message_count: messages.len(),
        messages: messages.iter().map(ExportedMessage::from).collect(),
    };

    serde_json::to_string_pretty(&document).map_err(|err| format!("序列化导出内容失败: {err}"))
}

/// 本地时间戳转成可读时间（导出用，不依赖时区库：按 UTC+8 折算太脆，直接用秒级时间戳的
/// 本地表示交给调用方？这里用 `chrono` 会多一个依赖，所以只用时间戳本身的日期部分）
fn format_timestamp(millis: i64) -> String {
    let total_seconds = millis.div_euclid(1000);
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);

    // 1970-01-01 起的天数换算成公历日期（导出只是给人看的，不追求时区精确）
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// Howard Hinnant 的 `civil_from_days`
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };

    (year + i64::from(month <= 2), month, day)
}

fn speaker(message: &ChatMessage) -> &'static str {
    match message.direction {
        MessageDirection::Outgoing => "我",
        MessageDirection::Incoming => "对方",
    }
}

fn render_text(messages: &[ChatMessage]) -> String {
    let mut output = String::new();

    for message in messages {
        output.push_str(&format!(
            "[{}] {}：{}\n",
            format_timestamp(message.created_at),
            speaker(message),
            describe(message),
        ));
    }

    output
}

/// 导出时怎么描述一条消息。附件只写「类型 + 文件名」，不把内容塞进 JSON / 文本里（§35）。
fn describe(message: &ChatMessage) -> String {
    if let Some(text) = message.text.as_deref() {
        return text.to_string();
    }

    let label = match message.kind {
        MessageKind::Image => "图片",
        MessageKind::File => "文件",
        MessageKind::Voice => "语音",
        MessageKind::Text => "文本",
    };

    match message
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.original_name.clone())
    {
        Some(name) => format!("[{label}] {name}"),
        None => format!("[{label}]"),
    }
}

fn render_markdown(messages: &[ChatMessage]) -> String {
    let mut output = String::from("# BongoCat 聊天记录\n\n");
    let mut current_day = String::new();

    for message in messages {
        let stamp = format_timestamp(message.created_at);
        let (day, clock) = stamp.split_at(10);

        if day != current_day {
            output.push_str(&format!("\n## {day}\n\n"));

            current_day = day.to_string();
        }

        output.push_str(&format!(
            "- **{}** {}：{}\n",
            speaker(message),
            clock.trim(),
            describe(message).replace('\n', "  \n  "),
        ));
    }

    output
}

/// 数据库句柄。连接包在 `Mutex` 里：所有命令都在 Rust 侧同步访问，量级很小。
pub struct PairHistory {
    connection: Mutex<Connection>,
}

impl PairHistory {
    /// 打开（首次运行时创建）本地聊天库
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("创建聊天数据库目录失败: {err}"))?;
        }

        let connection =
            Connection::open(path).map_err(|err| format!("打开聊天数据库失败: {err}"))?;

        Self::from_connection(connection)
    }

    /// 测试与降级用：不落盘
    pub fn in_memory() -> Result<Self, String> {
        let connection =
            Connection::open_in_memory().map_err(|err| format!("创建内存数据库失败: {err}"))?;

        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, String> {
        connection
            .execute_batch(SCHEMA)
            .map_err(|err| format!("初始化聊天数据库失败: {err}"))?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前记录周期（§36）
    pub fn epoch(&self) -> Result<i64, String> {
        let value: Option<String> = self
            .lock()
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![EPOCH_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| format!("读取记录周期失败: {err}"))?;

        Ok(value
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or(DEFAULT_EPOCH))
    }

    /// 开始新的记录周期，返回新周期号。旧消息默认保留（§36 不允许静默删聊天）。
    pub fn start_new_epoch(&self, delete_old: bool) -> Result<i64, String> {
        let next = self.epoch()? + 1;

        self.lock()
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![EPOCH_KEY, next.to_string()],
            )
            .map_err(|err| format!("写入记录周期失败: {err}"))?;

        if delete_old {
            // 先把还没送达的发出消息挪进新周期：它们是「对方还没收到」的待办，
            // 不能跟着旧记录一起删掉（§32 / §36：绝不静默丢聊天）
            self.carry_over_undelivered(next)?;
            self.delete_epochs_before(next)?;
        }

        Ok(next)
    }

    /// 把旧周期里还没送达的发出消息改挂到新周期，返回条数
    fn carry_over_undelivered(&self, epoch: i64) -> Result<usize, String> {
        self.lock()
            .execute(
                "UPDATE messages SET conversation_epoch = ?1
                 WHERE conversation_epoch < ?1
                   AND direction = 'outgoing'
                   AND status IN ('pending', 'sent')",
                params![epoch],
            )
            .map_err(|err| format!("保留未送达消息失败: {err}"))
    }

    fn delete_epochs_before(&self, epoch: i64) -> Result<usize, String> {
        self.lock()
            .execute(
                "DELETE FROM messages WHERE conversation_epoch < ?1",
                params![epoch],
            )
            .map_err(|err| format!("删除旧聊天记录失败: {err}"))
    }

    /// 写入一条消息。同一个 `id` 重复写入会被忽略（重发与去重都在上层，这里是兜底）。
    pub fn insert(&self, message: &NewMessage) -> Result<ChatMessage, String> {
        self.lock()
            .execute(
                "INSERT OR IGNORE INTO messages
                    (id, direction, kind, created_at, text, status, attachment_id, conversation_epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    message.id,
                    message.direction.as_str(),
                    message.kind.as_str(),
                    message.created_at,
                    message.text,
                    message.status.as_str(),
                    message.attachment_id,
                    message.conversation_epoch,
                ],
            )
            .map_err(|err| format!("写入聊天记录失败: {err}"))?;

        self.find(&message.id)?
            .ok_or_else(|| "写入聊天记录后读不回来".to_string())
    }

    pub fn find(&self, id: &str) -> Result<Option<ChatMessage>, String> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!("{MESSAGE_COLUMNS} WHERE id = ?1"))
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;
        let mut rows = statement
            .query_map(params![id], RawMessage::from_row)
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;

        match rows.next() {
            None => Ok(None),
            Some(row) => {
                let mut message = row
                    .map_err(|err| format!("读取聊天记录失败: {err}"))?
                    .into_message()?;

                fill_attachments(&connection, std::slice::from_mut(&mut message))?;

                Ok(Some(message))
            }
        }
    }

    /// 写入（或覆盖）一条附件记录。重发同一个附件时用覆盖，别留下两条孤儿记录。
    pub fn upsert_attachment(&self, attachment: &NewAttachment) -> Result<AttachmentRecord, String> {
        self.lock()
            .execute(
                "INSERT INTO attachments (id, kind, original_name, mime, size, sha256, local_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    kind = excluded.kind,
                    original_name = excluded.original_name,
                    mime = excluded.mime,
                    size = excluded.size,
                    sha256 = excluded.sha256,
                    local_path = excluded.local_path",
                params![
                    attachment.id,
                    attachment.kind.as_str(),
                    attachment.original_name,
                    attachment.mime,
                    attachment.size.map(|value| value as i64),
                    attachment.sha256,
                    attachment.local_path,
                    attachment.created_at,
                ],
            )
            .map_err(|err| format!("写入附件记录失败: {err}"))?;

        self.attachment(&attachment.id)?
            .ok_or_else(|| "写入附件记录后读不回来".to_string())
    }

    pub fn attachment(&self, id: &str) -> Result<Option<AttachmentRecord>, String> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!("{ATTACHMENT_COLUMNS} WHERE id = ?1"))
            .map_err(|err| format!("查询附件记录失败: {err}"))?;

        statement
            .query_row(params![id], RawAttachment::from_row)
            .optional()
            .map_err(|err| format!("查询附件记录失败: {err}"))?
            .map(RawAttachment::into_record)
            .transpose()
    }

    /// 收完（或取消、重发）之后更新本机落盘路径。
    ///
    /// 校验失败时要把路径清回 `None`，否则聊天窗口会一直显示一个已经不存在的文件。
    pub fn set_attachment_path(
        &self,
        id: &str,
        local_path: Option<&str>,
    ) -> Result<Option<AttachmentRecord>, String> {
        self.lock()
            .execute(
                "UPDATE attachments SET local_path = ?2 WHERE id = ?1",
                params![id, local_path],
            )
            .map_err(|err| format!("更新附件路径失败: {err}"))?;

        self.attachment(id)
    }

    /// 只在状态真的变化时返回新行，避免上层重复广播事件
    pub fn set_status(
        &self,
        id: &str,
        status: MessageStatus,
    ) -> Result<Option<ChatMessage>, String> {
        let changed = self
            .lock()
            .execute(
                "UPDATE messages SET status = ?2 WHERE id = ?1 AND status <> ?2",
                params![id, status.as_str()],
            )
            .map_err(|err| format!("更新聊天记录状态失败: {err}"))?;

        if changed == 0 {
            return Ok(None);
        }

        self.find(id)
    }

    /// 分页读取当前周期的历史：返回 `limit` 条以内、按时间升序的连续一段
    pub fn list(
        &self,
        epoch: i64,
        before: Option<i64>,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!(
                "{MESSAGE_COLUMNS} WHERE conversation_epoch = ?1 AND (?2 IS NULL OR seq < ?2)
                 ORDER BY seq DESC LIMIT ?3"
            ))
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;
        let rows = statement
            .query_map(params![epoch, before, limit + 1], RawMessage::from_row)
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;

        let mut messages = Vec::new();

        for row in rows {
            messages.push(
                row.map_err(|err| format!("读取聊天记录失败: {err}"))?
                    .into_message()?,
            );
        }

        let has_more = messages.len() as i64 > limit;

        messages.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        messages.reverse();
        fill_attachments(&connection, &mut messages)?;

        Ok(HistoryPage {
            messages,
            has_more,
            epoch,
        })
    }

    /// 还没送达的消息（`pending` / `sent`），按时间升序，用于重连后重发（§32）
    pub fn pending(&self, limit: usize) -> Result<Vec<ChatMessage>, String> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!(
                "{MESSAGE_COLUMNS} WHERE direction = 'outgoing' AND status IN ('pending', 'sent')
                 ORDER BY seq ASC LIMIT ?1"
            ))
            .map_err(|err| format!("查询待发送消息失败: {err}"))?;
        let rows = statement
            .query_map(
                params![i64::try_from(limit).unwrap_or(i64::MAX)],
                RawMessage::from_row,
            )
            .map_err(|err| format!("查询待发送消息失败: {err}"))?;

        let mut messages = Vec::new();

        for row in rows {
            messages.push(
                row.map_err(|err| format!("读取待发送消息失败: {err}"))?
                    .into_message()?,
            );
        }

        fill_attachments(&connection, &mut messages)?;

        Ok(messages)
    }

    /// 按周期统计条数；`epoch` 为 `None` 时统计全部
    pub fn count(&self, epoch: Option<i64>) -> Result<i64, String> {
        match epoch {
            Some(epoch) => self.lock().query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_epoch = ?1",
                params![epoch],
                |row| row.get(0),
            ),
            None => self
                .lock()
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0)),
        }
        .map_err(|err| format!("统计聊天记录失败: {err}"))
    }

    /// 按时间升序取出全部消息（导出用，不分周期）
    pub fn all(&self) -> Result<Vec<ChatMessage>, String> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!("{MESSAGE_COLUMNS} ORDER BY seq ASC"))
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;
        let rows = statement
            .query_map([], RawMessage::from_row)
            .map_err(|err| format!("查询聊天记录失败: {err}"))?;

        let mut messages = Vec::new();

        for row in rows {
            messages.push(
                row.map_err(|err| format!("读取聊天记录失败: {err}"))?
                    .into_message()?,
            );
        }

        fill_attachments(&connection, &mut messages)?;

        Ok(messages)
    }

    /// 输入统计（§33 的 `input_stats`）：同一天覆盖写最新值，跨天由调用方换 key
    pub fn upsert_input_stats(&self, date: &str, keyboard: u64, mouse: u64) -> Result<(), String> {
        self.lock()
            .execute(
                "INSERT INTO input_stats (date, keyboard_count, mouse_click_count)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(date) DO UPDATE SET
                    keyboard_count = excluded.keyboard_count,
                    mouse_click_count = excluded.mouse_click_count",
                params![
                    date,
                    i64::try_from(keyboard).unwrap_or(i64::MAX),
                    i64::try_from(mouse).unwrap_or(i64::MAX)
                ],
            )
            .map_err(|err| format!("写入输入统计失败: {err}"))?;

        Ok(())
    }

    /// 读回某一天记录的输入统计（§33）。目前只有写入路径在用，这里留给以后回看历史统计，
    /// 所以暂时允许不被引用。
    #[allow(dead_code)]
    pub fn input_stats(&self, date: &str) -> Result<Option<(u64, u64)>, String> {
        self.lock()
            .query_row(
                "SELECT keyboard_count, mouse_click_count FROM input_stats WHERE date = ?1",
                params![date],
                |row| {
                    Ok((
                        u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                        u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                    ))
                },
            )
            .optional()
            .map_err(|err| format!("读取输入统计失败: {err}"))
    }
}

const MESSAGE_COLUMNS: &str = "SELECT seq, id, direction, kind, created_at, text, status, attachment_id, conversation_epoch FROM messages";
const ATTACHMENT_COLUMNS: &str =
    "SELECT id, kind, original_name, mime, size, sha256, local_path, created_at FROM attachments";

/// 给附件消息补上附件记录：一次读一页，只对真正带附件的消息各查一次
fn fill_attachments(
    connection: &Connection,
    messages: &mut [ChatMessage],
) -> Result<(), String> {
    if !messages.iter().any(|message| message.attachment_id.is_some()) {
        return Ok(());
    }

    let mut cached: HashMap<String, Option<AttachmentRecord>> = HashMap::new();

    for message in messages.iter_mut() {
        let Some(id) = message.attachment_id.clone() else {
            continue;
        };

        if !cached.contains_key(&id) {
            let mut statement = connection
                .prepare(&format!("{ATTACHMENT_COLUMNS} WHERE id = ?1"))
                .map_err(|err| format!("查询附件记录失败: {err}"))?;
            let record = statement
                .query_row(params![id], RawAttachment::from_row)
                .optional()
                .map_err(|err| format!("查询附件记录失败: {err}"))?
                .map(RawAttachment::into_record)
                .transpose()?;

            cached.insert(id.clone(), record);
        }

        message.attachment = cached.get(&id).cloned().flatten();
    }

    Ok(())
}

/// 附件表的一行
struct RawAttachment {
    id: String,
    kind: String,
    original_name: Option<String>,
    mime: Option<String>,
    size: Option<i64>,
    sha256: Option<String>,
    local_path: Option<String>,
    created_at: i64,
}

impl RawAttachment {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            kind: row.get(1)?,
            original_name: row.get(2)?,
            mime: row.get(3)?,
            size: row.get(4)?,
            sha256: row.get(5)?,
            local_path: row.get(6)?,
            created_at: row.get(7)?,
        })
    }

    fn into_record(self) -> Result<AttachmentRecord, String> {
        Ok(AttachmentRecord {
            id: self.id,
            kind: MessageKind::parse(&self.kind)?,
            original_name: self.original_name,
            mime: self.mime,
            size: self.size.map(|value| value.max(0) as u64),
            sha256: self.sha256,
            local_path: self.local_path,
            created_at: self.created_at,
        })
    }
}

/// SQLite 里枚举存的是文本，所以先按原始类型取出来再解析
struct RawMessage {
    seq: i64,
    id: String,
    direction: String,
    kind: String,
    created_at: i64,
    text: Option<String>,
    status: String,
    attachment_id: Option<String>,
    conversation_epoch: i64,
}

impl RawMessage {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            seq: row.get(0)?,
            id: row.get(1)?,
            direction: row.get(2)?,
            kind: row.get(3)?,
            created_at: row.get(4)?,
            text: row.get(5)?,
            status: row.get(6)?,
            attachment_id: row.get(7)?,
            conversation_epoch: row.get(8)?,
        })
    }

    fn into_message(self) -> Result<ChatMessage, String> {
        Ok(ChatMessage {
            seq: self.seq,
            id: self.id,
            direction: MessageDirection::parse(&self.direction)?,
            kind: MessageKind::parse(&self.kind)?,
            created_at: self.created_at,
            text: self.text,
            status: MessageStatus::parse(&self.status)?,
            attachment_id: self.attachment_id,
            attachment: None,
            conversation_epoch: self.conversation_epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> PairHistory {
        PairHistory::in_memory().unwrap()
    }

    fn outgoing(history: &PairHistory, id: &str, text: &str, created_at: i64) -> ChatMessage {
        history
            .insert(&NewMessage::outgoing_text(
                id.to_string(),
                text.to_string(),
                created_at,
                1,
            ))
            .unwrap()
    }

    /// §36：删除旧记录时，还没送达的发出消息必须留下（它们还是「待对方收到」的活）
    #[test]
    fn starting_a_new_cycle_keeps_undelivered_messages() {
        let history = history();

        outgoing(&history, "m1", "已经送到了", 1);

        history.set_status("m1", MessageStatus::Delivered).unwrap();

        let unsent = outgoing(&history, "m2", "对方还没收到", 2);

        history
            .insert(&NewMessage::incoming_text(
                "m3".into(),
                "对方发来的".into(),
                3,
                1,
            ))
            .unwrap();

        assert_eq!(history.start_new_epoch(true).unwrap(), 2);

        let carried = history.find(&unsent.id).unwrap().unwrap();

        assert_eq!(carried.conversation_epoch, 2, "未送达的消息要跟着进新周期");
        assert_eq!(history.pending(10).unwrap().len(), 1);
        assert!(
            history.find("m1").unwrap().is_none(),
            "已送达的旧消息应当删掉"
        );
        assert!(
            history.find("m3").unwrap().is_none(),
            "收到的旧消息应当删掉"
        );
        assert_eq!(history.count(None).unwrap(), 1);
    }

    #[test]
    fn inserts_and_reads_back_a_message() {
        let history = history();
        let stored = outgoing(&history, "m1", "你好", 1_700_000_000_000);

        assert!(stored.seq > 0);
        assert_eq!(stored.status, MessageStatus::Pending);
        assert_eq!(stored.direction, MessageDirection::Outgoing);
        assert_eq!(stored.kind, MessageKind::Text);
        assert_eq!(stored.text.as_deref(), Some("你好"));
        assert_eq!(history.find("m1").unwrap().unwrap().id, "m1");
    }

    #[test]
    fn inserting_the_same_id_twice_keeps_the_first_row() {
        let history = history();
        let first = outgoing(&history, "m1", "第一条", 1);
        let second = history
            .insert(&NewMessage::incoming_text("m1".into(), "重复".into(), 2, 1))
            .unwrap();

        assert_eq!(first.seq, second.seq);
        assert_eq!(second.text.as_deref(), Some("第一条"));
        assert_eq!(history.count(None).unwrap(), 1);
    }

    #[test]
    fn status_only_reports_real_changes() {
        let history = history();

        outgoing(&history, "m1", "hello", 1);

        assert_eq!(
            history
                .set_status("m1", MessageStatus::Sent)
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Sent
        );
        // 同一个状态再写一次不该广播事件
        assert!(
            history
                .set_status("m1", MessageStatus::Sent)
                .unwrap()
                .is_none()
        );
        assert!(
            history
                .set_status("缺省", MessageStatus::Sent)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            history
                .set_status("m1", MessageStatus::Delivered)
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Delivered
        );
    }

    #[test]
    fn list_pages_backwards_with_a_cursor() {
        let history = history();

        for index in 0..5 {
            outgoing(
                &history,
                &format!("m{index}"),
                &format!("第 {index} 条"),
                index as i64,
            );
        }

        let page = history.list(1, None, 2).unwrap();

        assert_eq!(page.messages.len(), 2);
        assert!(page.has_more);
        // 升序返回：拿到的是最新的两条
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m3", "m4"]
        );

        let older = history.list(1, Some(page.messages[0].seq), 2).unwrap();

        assert_eq!(
            older
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        assert!(older.has_more);

        let oldest = history.list(1, Some(older.messages[0].seq), 2).unwrap();

        assert_eq!(oldest.messages.len(), 1);
        assert!(!oldest.has_more);
    }

    #[test]
    fn list_only_returns_the_current_epoch() {
        let history = history();

        outgoing(&history, "old", "上一周期", 1);

        let next = history.start_new_epoch(false).unwrap();

        assert_eq!(next, 2);
        assert_eq!(history.epoch().unwrap(), 2);
        assert!(history.list(2, None, 10).unwrap().messages.is_empty());
        assert_eq!(history.list(1, None, 10).unwrap().messages.len(), 1);
        // 旧周期默认保留，显式删除时才清掉（还没送达的消息另有专门用例，见
        // `starting_a_new_cycle_keeps_undelivered_messages`）
        history.set_status("old", MessageStatus::Delivered).unwrap();
        history.start_new_epoch(true).unwrap();
        assert_eq!(history.count(None).unwrap(), 0);
    }

    #[test]
    fn pending_lists_only_undelivered_outgoing_messages_in_order() {
        let history = history();

        outgoing(&history, "m1", "第一条", 1);
        outgoing(&history, "m2", "第二条", 2);
        history
            .insert(&NewMessage::incoming_text("*".into(), "对方".into(), 3, 1))
            .unwrap();
        history.set_status("m2", MessageStatus::Delivered).unwrap();

        let pending = history.pending(10).unwrap();

        assert_eq!(
            pending
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1"]
        );

        history.set_status("m1", MessageStatus::Sent).unwrap();
        assert_eq!(
            history.pending(10).unwrap().len(),
            1,
            "已发出但没收到 ack 的仍要重发"
        );

        history.set_status("m1", MessageStatus::Failed).unwrap();
        assert!(history.pending(10).unwrap().is_empty());
    }

    #[test]
    fn input_stats_are_upserted_per_day() {
        let history = history();

        history.upsert_input_stats("2026-09-23", 10, 2).unwrap();
        history.upsert_input_stats("2026-09-23", 12, 3).unwrap();

        assert_eq!(history.input_stats("2026-09-23").unwrap(), Some((12, 3)));
        assert_eq!(history.input_stats("2026-09-24").unwrap(), None);
    }

    #[test]
    fn json_export_round_trips_every_message() {
        let history = history();

        outgoing(&history, "m1", "你好", 0);
        history
            .insert(&NewMessage::incoming_text(
                "m2".into(),
                "你也好".into(),
                1,
                1,
            ))
            .unwrap();

        let json = history
            .all()
            .unwrap()
            .iter()
            .map(|message| message.clone())
            .collect::<Vec<_>>();
        let rendered = ExportFormat::Json.render(&json, 42).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed["messageCount"], 2);
        assert_eq!(parsed["messages"][0]["text"], "你好");
        assert_eq!(parsed["messages"][1]["direction"], "incoming");
    }

    #[test]
    fn text_and_markdown_exports_are_readable() {
        let history = history();

        outgoing(&history, "m1", "你好", 1_700_000_000_000);
        history
            .insert(&NewMessage::incoming_text(
                "m2".into(),
                "你也好".into(),
                1_700_000_060_000,
                1,
            ))
            .unwrap();

        let messages = history.all().unwrap();
        let text = ExportFormat::Txt.render(&messages, 0).unwrap();
        let markdown = ExportFormat::Md.render(&messages, 0).unwrap();

        assert!(text.contains("我：你好"));
        assert!(text.contains("对方：你也好"));
        assert!(markdown.contains("# BongoCat 聊天记录"));
        assert!(markdown.contains("**我**"));
        assert!(markdown.contains("## 2023-11-14"));
    }

    #[test]
    fn timestamp_formatting_matches_a_known_date() {
        // 1_700_000_000_000 ms = 2023-11-14 22:13:20 UTC
        assert_eq!(format_timestamp(1_700_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
    }

    fn attachment(id: &str, kind: MessageKind, name: &str, size: u64) -> NewAttachment {
        NewAttachment {
            id: id.into(),
            kind,
            original_name: Some(name.into()),
            mime: Some("image/png".into()),
            size: Some(size),
            sha256: Some("a".repeat(64)),
            local_path: None,
            created_at: 1_700_000_000_000,
        }
    }

    /// 附件消息读回来要带上附件记录，前端才不用为每条消息再查一次
    #[test]
    fn attachment_messages_carry_their_record() {
        let history = history();

        history
            .upsert_attachment(&attachment("a1", MessageKind::Image, "猫.png", 2048))
            .unwrap();
        history
            .insert(&NewMessage::outgoing_attachment(
                "m1".into(),
                MessageKind::Image,
                "a1".into(),
                1_700_000_000_000,
                1,
            ))
            .unwrap();

        let message = history.find("m1").unwrap().unwrap();

        assert_eq!(message.kind, MessageKind::Image);
        assert!(message.text.is_none());
        assert_eq!(
            message.attachment.as_ref().map(|record| record.original_name.as_deref()),
            Some(Some("猫.png"))
        );
        assert_eq!(message.attachment.as_ref().and_then(|r| r.size), Some(2048));
        assert!(message.attachment.as_ref().unwrap().local_path.is_none());

        // 分页与导出走的是同一套填充逻辑
        let page = history.list(1, None, 10).unwrap();

        assert_eq!(page.messages.len(), 1);
        assert!(page.messages[0].attachment.is_some());

        let exported = history.all().unwrap();

        assert!(exported[0].attachment.is_some());
    }

    /// §78：导出的 JSON 也不能带上本机落盘路径（附件记录里的 `localPath`），
    /// 导出的文件是拿去备份甚至分享的，带上它等于把 `C:\Users\...` 一起交出去
    #[test]
    fn json_export_never_carries_local_paths() {
        let history = history();

        history
            .upsert_attachment(&attachment("a1", MessageKind::File, "报告.pdf", 100))
            .unwrap();
        history
            .set_attachment_path("a1", Some(r"C:\Users\cat\AppData\Local\BongoCat\attachments\a1.pdf"))
            .unwrap();
        history
            .insert(&NewMessage::outgoing_attachment(
                "m1".into(),
                MessageKind::File,
                "a1".into(),
                1,
                1,
            ))
            .unwrap();

        let messages = history.all().unwrap();

        assert!(
            messages[0]
                .attachment
                .as_ref()
                .unwrap()
                .local_path
                .is_some(),
            "库里要留着路径：聊天窗口还要用它预览与「另存为」"
        );

        let json = ExportFormat::Json.render(&messages, 0).unwrap();

        assert!(!json.contains("Users"), "导出的 JSON 带上了本机路径：{json}");
        assert!(json.contains(r#""originalName": "报告.pdf""#), "{json}");
    }

    /// 传输中途 / 失败 / 重发都会改写落盘路径，覆盖写不能留下两条记录
    #[test]
    fn attachment_path_is_updated_in_place() {
        let history = history();

        history
            .upsert_attachment(&attachment("a1", MessageKind::File, "报告.pdf", 100))
            .unwrap();

        let stored = history
            .set_attachment_path("a1", Some(r"C:\cache\abc.pdf"))
            .unwrap()
            .unwrap();

        assert_eq!(stored.local_path.as_deref(), Some(r"C:\cache\abc.pdf"));

        // 重发时先清空，再重新覆盖记录
        let cleared = history.set_attachment_path("a1", None).unwrap().unwrap();

        assert!(cleared.local_path.is_none());

        history
            .upsert_attachment(&attachment("a1", MessageKind::File, "报告-v2.pdf", 200))
            .unwrap();

        let overwritten = history.attachment("a1").unwrap().unwrap();

        assert_eq!(overwritten.original_name.as_deref(), Some("报告-v2.pdf"));
        assert_eq!(overwritten.size, Some(200));
        assert!(history.attachment("missing").unwrap().is_none());
    }

    /// 附件消息在 TXT / Markdown 里要有可读的描述，而不是空行
    #[test]
    fn exports_describe_attachments() {
        let history = history();

        history
            .upsert_attachment(&attachment("a1", MessageKind::Image, "猫.png", 2048))
            .unwrap();
        history
            .insert(&NewMessage::incoming_attachment(
                "m1".into(),
                MessageKind::Image,
                "a1".into(),
                1_700_000_000_000,
                1,
            ))
            .unwrap();

        let messages = history.all().unwrap();
        let text = ExportFormat::Txt.render(&messages, 0).unwrap();

        assert!(text.contains("对方：[图片] 猫.png"), "{text}");
    }
}

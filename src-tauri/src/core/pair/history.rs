//! 本地聊天数据库（SQLite）。
//!
//! 聊天记录不进 Pinia，也不进设置文件：读写全在这里（见 docs/pair-plan.md §33）。
//! 表一次建全（`messages` / `attachments` / `input_stats` / `metadata`），附件列先留着，
//! Phase 5 直接用同一个库，不需要再迁移。
//!
//! 这个模块也是**纯存储**：不知道网络、不知道 Tauri，方便直接跑单测。

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
    const fn as_str(self) -> &'static str {
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
    pub conversation_epoch: i64,
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
    messages: &'a [ChatMessage],
}

fn render_json(messages: &[ChatMessage], exported_at: i64) -> Result<String, String> {
    let document = JsonExport {
        format_version: 1,
        exported_at,
        message_count: messages.len(),
        messages,
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
            message.text.as_deref().unwrap_or("（非文本消息）"),
        ));
    }

    output
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
            message
                .text
                .as_deref()
                .unwrap_or("（非文本消息）")
                .replace('\n', "  \n  "),
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
            Some(row) => Ok(Some(
                row.map_err(|err| format!("读取聊天记录失败: {err}"))?
                    .into_message()?,
            )),
        }
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
}

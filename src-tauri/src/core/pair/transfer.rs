//! 附件传输管线（§37 - §43）。
//!
//! 这个模块只管「一块文件怎么切、怎么校验、怎么落到磁盘」，不知道 WebSocket、
//! 不知道 Tauri 事件，也不做限速（帧级限速在 `manager.rs` 的 `Pacer` 里）。
//! 因此它可以脱离连接直接跑单测。
//!
//! 约定（R17 / §41）：
//!
//! ```text
//! 发送：<source> --512 KiB/chunk--> 加密帧（每帧 14 字节帧头带 transferId 与 chunk 序号）
//! 接收：边收边写 <root>/tmp/<uuid>.part，收完校验 SHA-256 再 rename 到 <root>/attachments
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind, Read, Seek as _, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::protocol::{FRAME_HEADER_SIZE, MAX_BINARY_FRAME_SIZE, NONCE_SIZE, TransferKind};

/// 中继那条路上每个 chunk 的明文大小（§40）
pub const CHUNK_SIZE: usize = 512 * 1024;
/// P2P（DataChannel）下每个 chunk 的明文大小（§7 / R22 / R32）。
///
/// 48 KiB 是保守默认：`webrtc` 0.21 的 SCTP 单条消息上限实测是 256 KiB（超限前由实现自己
/// 分片，见 R22），48 KiB 也给「一块一帧」留出足够余量。
pub const P2P_CHUNK_SIZE: usize = 48 * 1024;
/// `chunk_size` 的下界（§7 / R32）：不设下界的话，对端把 `chunk_size` 报成 1 就能逼你在
/// 本地写十亿次小文件。
pub const MIN_CHUNK_SIZE: usize = 4 * 1024;
/// Poly1305 认证标签的长度
const AEAD_TAG_SIZE: usize = 16;

/// `chunk_size` 的合法上界（§7 / R32）：既不能超过中继允许的一帧，也不能超过我们自己封
/// 出来的一帧（帧头 + nonce + tag 都是封帧开销）。
///
/// **发送侧的夹紧与接收侧的校验必须调同一个函数**：两侧一旦用了不同的分块长度，接收侧
/// 每一块都会报「附件分片大小不对」，比直接拒绝 offer 更难查。
pub fn max_chunk_size() -> usize {
    CHUNK_SIZE.min(MAX_BINARY_FRAME_SIZE - FRAME_HEADER_SIZE - NONCE_SIZE - AEAD_TAG_SIZE)
}

/// 对端 offer 里的 `chunk_size` 是否在合法范围内（§7 / R32 的接收侧校验）
pub fn chunk_size_is_valid(chunk_size: u64) -> bool {
    (MIN_CHUNK_SIZE as u64..=max_chunk_size() as u64).contains(&chunk_size)
}
/// 附件默认上限（§42）：256 MB
pub const DEFAULT_MAX_SIZE: u64 = 256 * 1024 * 1024;
/// 附件硬上限（§42）：设置填得再大也不会超过它
pub const HARD_MAX_SIZE: u64 = 1024 * 1024 * 1024;
/// 超过这个大小、且不是图片/语音的普通文件，接收前先问用户（§42）
pub const LARGE_FILE_CONFIRM_SIZE: u64 = 50 * 1024 * 1024;
/// 保留的文件名最长字符数（只是显示与「另存为」的默认值，落盘永远用 UUID）
const MAX_NAME_CHARS: usize = 120;
/// 落盘扩展名最长的字符数
const MAX_EXTENSION_CHARS: usize = 10;
/// 启动时清理多久以前的临时残留
const STALE_PART_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// 一个附件按 `chunk_size` 需要切成几块。空文件是 0 块（offer 之后直接 complete）。
///
/// `chunk_size` 由调用方保证在 `[MIN_CHUNK_SIZE, max_chunk_size()]` 内；这里再夹一层 1
/// 只是为了让除法不会 panic（0 会让 `div_ceil` 直接炸）。
pub fn chunk_count(size: u64, chunk_size: usize) -> u32 {
    size.div_ceil(chunk_size.max(1) as u64) as u32
}

/// 第 `index` 块按 `chunk_size` 应该是多少字节
pub fn chunk_length(size: u64, index: u32, chunk_size: usize) -> usize {
    let chunk_size = chunk_size.max(1);
    let start = index as u64 * chunk_size as u64;

    if start >= size {
        return 0;
    }

    (size - start).min(chunk_size as u64) as usize
}

/// 把对方给的文件名洗成可以安全显示与另存的名字（§42）。
///
/// 只保留最后一段（同时挡掉 `/` 与 `\` 两种分隔符），去掉控制字符和 Windows
/// 不允许出现在文件名里的字符，并避免 `.` / `..` 这类「整段都是点」的名字。
/// 落盘时用的是 UUID，这个字符串只用于显示和「另存为」的默认文件名。
pub fn sanitize_file_name(name: &str) -> String {
    let last = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .filter(|character| !matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect::<String>();

    let trimmed = last.trim().trim_matches('.').trim();

    if trimmed.is_empty() {
        return "attachment".to_string();
    }

    let sanitized = truncate_name(trimmed);

    // Windows 保留设备名在任何目录下都指向设备，用户拿它「另存为」一定失败（§42）
    if is_reserved_name(&sanitized) {
        return format!("_{sanitized}");
    }

    sanitized
}

/// `CON` / `NUL` / `COM1` / `LPT1` 这类整段（或点号前的主干）是设备名的名字
fn is_reserved_name(name: &str) -> bool {
    let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
    let upper = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();

    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || is_reserved_numbered(&upper, "COM")
        || is_reserved_numbered(&upper, "LPT")
}

fn is_reserved_numbered(upper: &str, prefix: &str) -> bool {
    matches!(
        upper.strip_prefix(prefix),
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
    )
}

/// 保留扩展名的截断：太长时截前面的主干，别把扩展名切掉
fn truncate_name(name: &str) -> String {
    if name.chars().count() <= MAX_NAME_CHARS {
        return name.to_string();
    }

    let extension = sanitize_extension(name);
    let keep = MAX_NAME_CHARS - (extension.chars().count() + 1);
    let stem: String = name.chars().take(keep).collect();

    if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{extension}")
    }
}

/// 落盘用的扩展名：只接受短的 ascii 字母数字，其余一律丢掉（§42：扩展名不可信）
pub fn sanitize_extension(name: &str) -> String {
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    if extension.is_empty()
        || extension.chars().count() > MAX_EXTENSION_CHARS
        || !extension.chars().all(|character| character.is_ascii_alphanumeric())
    {
        return String::new();
    }

    extension.to_ascii_lowercase()
}

/// MIME 只是显示提示，永不用于决定「要不要打开/执行」（§42）
pub fn sanitize_mime(mime: &str) -> String {
    let candidate = mime.trim();
    let mut parts = candidate.split('/');

    let (Some(top), Some(sub), None) = (parts.next(), parts.next(), parts.next()) else {
        return "application/octet-stream".to_string();
    };

    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 64
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "!#$&^_.+-".contains(character))
    };

    if valid(top) && valid(sub) {
        candidate.to_ascii_lowercase()
    } else {
        "application/octet-stream".to_string()
    }
}

fn to_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 边读边算，不把整个文件塞进内存（§41 的精神同样适用于发送侧）
pub fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let file = File::open(path).map_err(|error| format!("读取附件失败: {error}"))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("读取附件失败: {error}"))?;

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
        size += read as u64;
    }

    Ok((to_hex(&hasher.finalize()), size))
}

/// 发送侧的一次传输
pub struct OutgoingTransfer {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    /// 这一次传输实际使用的分片大小（§7 / R22）：中继 512 KiB、P2P 48 KiB。
    /// 读块、seek 与「下一块该多少字节」都必须用它，不能用全局常量。
    pub chunk_size: usize,
    pub chunks: u32,
    pub bytes_sent: u64,
    pub sent_chunks: u32,
    source: PathBuf,
}

impl OutgoingTransfer {
    /// 用已经算好的 SHA-256 构造（发送前先落库：库里的 size/sha256 就是这里的值）。
    /// 哈希由调用方在 `spawn_blocking` 里算好（大文件的哈希是唯一可能明显耗时的文件操作）。
    pub fn with_digest(
        source: PathBuf,
        name: &str,
        mime: &str,
        size: u64,
        sha256: String,
        chunk_size: usize,
    ) -> Self {
        let chunk_size = chunk_size.clamp(MIN_CHUNK_SIZE, max_chunk_size());

        Self {
            name: sanitize_file_name(name),
            mime: sanitize_mime(mime),
            size,
            sha256,
            chunk_size,
            chunks: chunk_count(size, chunk_size),
            bytes_sent: 0,
            sent_chunks: 0,
            source,
        }
    }

    pub fn next_index(&self) -> u32 {
        self.sent_chunks
    }

    pub fn is_done(&self) -> bool {
        self.sent_chunks >= self.chunks
    }

    /// 下一块应该发多少字节（用于和接收方对齐校验）
    pub fn expected_length(&self) -> usize {
        chunk_length(self.size, self.sent_chunks, self.chunk_size)
    }

    pub fn read_chunk(&self, index: u32) -> Result<Vec<u8>, String> {
        let expected = chunk_length(self.size, index, self.chunk_size);

        if expected == 0 {
            return Ok(Vec::new());
        }

        let mut file =
            File::open(&self.source).map_err(|error| format!("读取附件失败: {error}"))?;

        file.seek_relative(index as i64 * self.chunk_size as i64)
            .map_err(|error| format!("读取附件失败: {error}"))?;

        let mut buffer = vec![0u8; expected];

        file.read_exact(&mut buffer)
            .map_err(|error| format!("读取附件失败: {error}"))?;

        Ok(buffer)
    }

    /// 一块发完（`sent` 表示真的写进了 socket），进度与序号同步前进
    pub fn mark_sent(&mut self, bytes: usize) {
        self.bytes_sent = (self.bytes_sent + bytes as u64).min(self.size);
        self.sent_chunks = (self.sent_chunks + 1).min(self.chunks);
    }

}

/// 接收侧的一次传输：边收边写 `.part`（§41）
pub struct IncomingTransfer {
    pub size: u64,
    pub sha256: String,
    /// 这次传输实际使用的分片大小：**取 offer 里的值**，不是本机常量（§7 / R22）
    pub chunk_size: usize,
    pub chunks: u32,
    pub received_chunks: u32,
    pub received_bytes: u64,
    part: PathBuf,
    file: Option<File>,
    hasher: Sha256,
}

impl IncomingTransfer {
    pub fn create(
        tmp_dir: &Path,
        size: u64,
        sha256: &str,
        chunks: u32,
        chunk_size: usize,
    ) -> Result<Self, String> {
        fs::create_dir_all(tmp_dir).map_err(|error| format!("创建临时目录失败: {error}"))?;

        let part = tmp_dir.join(format!("{}.part", Uuid::new_v4()));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part)
            .map_err(|error| format!("创建临时文件失败: {error}"))?;

        Ok(Self {
            size,
            sha256: sha256.to_ascii_lowercase(),
            chunk_size,
            chunks,
            received_chunks: 0,
            received_bytes: 0,
            part,
            file: Some(file),
            hasher: Sha256::new(),
        })
    }

    /// 写入一块。V1 不做乱序与断点续传（§43），所以序号必须严格递增。
    pub fn write_chunk(&mut self, index: u32, bytes: &[u8]) -> Result<(), String> {
        if index != self.received_chunks {
            return Err(format!(
                "附件分片顺序不对：期望第 {} 块，收到第 {index} 块",
                self.received_chunks
            ));
        }

        if index >= self.chunks {
            return Err("收到的附件分片超出 offer 声明的数量".to_string());
        }

        let expected = chunk_length(self.size, index, self.chunk_size);

        if bytes.len() != expected {
            return Err(format!(
                "附件分片大小不对：期望 {expected} 字节，收到 {} 字节",
                bytes.len()
            ));
        }

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| "这次附件传输已经结束了".to_string())?;

        file.write_all(bytes)
            .map_err(|error| format!("写入临时文件失败: {error}"))?;

        self.hasher.update(bytes);
        self.received_chunks += 1;
        self.received_bytes += bytes.len() as u64;

        Ok(())
    }

    /// 校验并落到附件目录，返回最终路径。
    ///
    /// 校验失败会删掉 `.part` 并返回错误（§41：错误就删掉、标记 failed，不要留半成品）。
    pub fn finish(mut self, target: PathBuf) -> Result<(PathBuf, u64, String), String> {
        let actual_size = self.received_bytes;
        let actual_sha256 = to_hex(&self.hasher.clone().finalize());

        // 先关掉文件句柄，Windows 上 rename 前必须先释放
        self.file = None;

        if self.received_chunks != self.chunks || actual_size != self.size {
            self.remove_part();

            return Err(format!(
                "附件没有收完：声明 {} 块 / {size} 字节，实际 {} 块 / {actual_size} 字节",
                self.chunks,
                self.received_chunks,
                size = self.size
            ));
        }

        if actual_sha256 != self.sha256 {
            self.remove_part();

            return Err("附件校验失败（SHA-256 不一致），已删除临时文件".to_string());
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("创建附件目录失败: {error}"))?;
        }

        fs::rename(&self.part, &target).map_err(|error| {
            self.remove_part();

            format!("保存附件失败: {error}")
        })?;

        Ok((target, actual_size, actual_sha256))
    }

    /// 中途取消 / 连接断开（§43）：V1 不做断点续传，直接丢掉半成品
    pub fn abort(mut self) {
        self.file = None;
        self.remove_part();
    }

    fn remove_part(&self) {
        let _ = fs::remove_file(&self.part);
    }

}

/// 附件与临时文件的落盘位置（§33 / §41）
#[derive(Debug, Clone)]
pub struct TransferStore {
    root: PathBuf,
}

impl TransferStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join("attachments")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn ensure(&self) -> Result<(), String> {
        for dir in [self.root.clone(), self.attachments_dir(), self.tmp_dir()] {
            fs::create_dir_all(&dir).map_err(|error| format!("创建附件目录失败: {error}"))?;
        }

        self.cleanup_stale_parts();

        Ok(())
    }

    /// 落盘文件名永远是 UUID（§42），扩展名只用来让系统认识这个文件
    pub fn attachment_name(&self, original: &str) -> String {
        let extension = sanitize_extension(original);

        if extension.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            format!("{}.{extension}", Uuid::new_v4())
        }
    }

    pub fn attachment_path(&self, file_name: &str) -> PathBuf {
        self.attachments_dir().join(file_name)
    }

    /// 粘贴的图片、录音这类「只在内存里有」的内容：写进附件目录，再删掉临时来源
    pub fn stage_copy(&self, source: &Path, original: &str) -> Result<PathBuf, String> {
        self.ensure()?;

        let target = self.attachment_path(&self.attachment_name(original));

        if fs::copy(source, &target).is_err() {
            // 跨盘或权限问题：退回到读进内存再写
            let bytes = fs::read(source).map_err(|error| format!("读取附件失败: {error}"))?;

            fs::write(&target, bytes).map_err(|error| format!("保存附件失败: {error}"))?;
        }

        match fs::remove_file(source) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }

        Ok(target)
    }

    /// 上一次运行留下的「录完待确认」wav（R41）：`voice-*.wav`。
    ///
    /// 它们只在 `PairRecording` 的内存状态里被认领，进程一退就没人管了
    /// （`cleanup_stale_parts` 只认 `.part`），所以**只在启动时**清一次。
    /// 运行中绝不能调它：那会把用户手上那条待确认的录音删掉。
    pub fn cleanup_orphan_recordings(&self) {
        let Ok(entries) = fs::read_dir(self.tmp_dir()) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with("voice-")
                && path.extension().and_then(|value| value.to_str()) == Some("wav")
            {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// 崩溃 / 强杀留下的 `.part`：只清理明显过期的，避免误删正在传输的文件
    fn cleanup_stale_parts(&self) {
        let Ok(entries) = fs::read_dir(self.tmp_dir()) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|value| value.to_str()) != Some("part") {
                continue;
            }

            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .map(|age| age > STALE_PART_AGE)
                        .unwrap_or(false)
                })
                .unwrap_or(false);

            if stale {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// 接收方是否还需要先问用户（§42）：普通文件、且超过阈值
pub fn needs_confirmation(kind: TransferKind, size: u64) -> bool {
    kind == TransferKind::File && size > LARGE_FILE_CONFIRM_SIZE
}

/// 设置里填的 MB 值 → 实际允许的字节数，并按硬上限夹紧
pub fn clamp_limit(max_mb: u64) -> u64 {
    max_mb.max(1).saturating_mul(1024 * 1024).min(HARD_MAX_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bongo-cat-pair-{tag}-{}", Uuid::new_v4()));

        fs::create_dir_all(&dir).unwrap();

        dir
    }

    fn write_source(dir: &Path, name: &str, size: usize) -> PathBuf {
        let path = dir.join(name);
        let bytes: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();

        fs::write(&path, bytes).unwrap();

        path
    }

    #[test]
    fn sanitizes_unsafe_names() {
        assert_eq!(sanitize_file_name("secret.zip"), "secret.zip");
        assert_eq!(sanitize_file_name(r"C:\Users\cat\Desktop\secret.zip"), "secret.zip");
        assert_eq!(sanitize_file_name("/home/cat/照片.png"), "照片.png");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("...."), "attachment");
        assert_eq!(sanitize_file_name(""), "attachment");
        assert_eq!(sanitize_file_name("a\u{0}b\u{7}.txt"), "ab.txt");
        assert_eq!(sanitize_file_name("bad:name?.txt"), "badname.txt");

        // Windows 保留设备名：落盘用的是 UUID，但「另存为」的默认名必须能存下去
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("con.txt"), "_con.txt");
        assert_eq!(sanitize_file_name("COM1.zip"), "_COM1.zip");
        assert_eq!(sanitize_file_name(" nUl "), "_nUl");
        assert_eq!(sanitize_file_name("COM10.log"), "COM10.log");
        assert_eq!(sanitize_file_name("console.txt"), "console.txt");

        // 保留扩展名的截断
        let long = format!("{}.png", "x".repeat(300));
        let truncated = sanitize_file_name(&long);

        assert!(truncated.chars().count() <= MAX_NAME_CHARS);
        assert!(truncated.ends_with(".png"));
    }

    #[test]
    fn sanitizes_extensions_and_mime() {
        assert_eq!(sanitize_extension("a.PNG"), "png");
        assert_eq!(sanitize_extension("a.verylongextension"), "");
        assert_eq!(sanitize_extension("a."), "");
        assert_eq!(sanitize_extension("noext"), "");
        assert_eq!(sanitize_extension("a.j/../p"), "");
        assert_eq!(sanitize_extension("a.é"), "");

        assert_eq!(sanitize_mime("image/png"), "image/png");
        assert_eq!(sanitize_mime("IMAGE/PNG"), "image/png");
        assert_eq!(sanitize_mime("application/octet-stream; echo hi"), "application/octet-stream");
        assert_eq!(sanitize_mime(""), "application/octet-stream");
        assert_eq!(sanitize_mime("text"), "application/octet-stream");
    }

    #[test]
    fn chunk_math_covers_the_boundaries() {
        assert_eq!(chunk_count(0, CHUNK_SIZE), 0);
        assert_eq!(chunk_count(1, CHUNK_SIZE), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64, CHUNK_SIZE), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64 + 1, CHUNK_SIZE), 2);
        assert_eq!(chunk_count(10 * 1024 * 1024, CHUNK_SIZE), 20);

        assert_eq!(chunk_length(0, 0, CHUNK_SIZE), 0);
        assert_eq!(chunk_length(1, 0, CHUNK_SIZE), 1);
        assert_eq!(chunk_length(CHUNK_SIZE as u64, 0, CHUNK_SIZE), CHUNK_SIZE);
        assert_eq!(chunk_length(CHUNK_SIZE as u64 + 1, 1, CHUNK_SIZE), 1);
        assert_eq!(
            chunk_length(CHUNK_SIZE as u64 + 1, 0, CHUNK_SIZE),
            CHUNK_SIZE
        );

        // §7 / R22：同一份尺寸换个 chunk_size，块数必须跟着变
        assert_eq!(chunk_count(10 * 1024 * 1024, P2P_CHUNK_SIZE), 214);
        assert_eq!(
            chunk_length(10 * 1024 * 1024, 213, P2P_CHUNK_SIZE),
            10 * 1024 * 1024 - 213 * P2P_CHUNK_SIZE
        );
        assert_eq!(chunk_length(10 * 1024 * 1024, 214, P2P_CHUNK_SIZE), 0);
    }

    /// §7 / R32：两侧必须用**同一对常量**。这里把范围与两个实际用到的值钉住，
    /// 免得哪天改了一侧忘了另一侧。
    #[test]
    fn the_chunk_size_range_covers_both_real_sizes() {
        assert!(chunk_size_is_valid(CHUNK_SIZE as u64));
        assert!(chunk_size_is_valid(P2P_CHUNK_SIZE as u64));

        assert!(!chunk_size_is_valid(0));
        assert!(!chunk_size_is_valid((MIN_CHUNK_SIZE - 1) as u64));
        assert!(!chunk_size_is_valid((max_chunk_size() + 1) as u64));

        // 上界要能把 512 KiB + 帧头 / nonce / tag 放进一帧里
        assert!(max_chunk_size() >= CHUNK_SIZE);
        assert!(max_chunk_size() < MAX_BINARY_FRAME_SIZE);
    }

    /// §81 的尺寸表：0 字节 / 1 字节 / 512 KiB / 512 KiB + 1 / 10 MB。
    /// **两种 chunk_size 都要走一遍**（R22 / R32）：中继 512 KiB、P2P 48 KiB。
    #[test]
    fn transfers_round_trip_every_size_the_plan_asks_for() {
        let root = temp_dir("round-trip");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        for chunk_size in [CHUNK_SIZE, P2P_CHUNK_SIZE] {
            for size in [0usize, 1, CHUNK_SIZE, CHUNK_SIZE + 1, 10 * 1024 * 1024] {
                let source = write_source(&root, &format!("src-{size}.bin"), size);
                let (sha256, size_on_disk) = sha256_file(&source).unwrap();
                let mut outgoing = OutgoingTransfer::with_digest(
                    source.clone(),
                    "src.bin",
                    "application/octet-stream",
                    size_on_disk,
                    sha256,
                    chunk_size,
                );

                assert_eq!(outgoing.size, size as u64);
                assert_eq!(outgoing.chunk_size, chunk_size);
                assert_eq!(outgoing.chunks, chunk_count(size as u64, chunk_size));

                let mut incoming = IncomingTransfer::create(
                    &store.tmp_dir(),
                    outgoing.size,
                    &outgoing.sha256,
                    outgoing.chunks,
                    chunk_size,
                )
                .unwrap();

                while !outgoing.is_done() {
                    let index = outgoing.next_index();
                    let chunk = outgoing.read_chunk(index).unwrap();

                    assert_eq!(chunk.len(), outgoing.expected_length());

                    incoming.write_chunk(index, &chunk).unwrap();
                    outgoing.mark_sent(chunk.len());
                }

                assert!(outgoing.is_done());

                let target = store.attachment_path(&store.attachment_name("src.bin"));
                let (saved, saved_size, saved_sha) = incoming.finish(target.clone()).unwrap();

                assert_eq!(saved, target);
                assert_eq!(saved_size, size as u64);
                assert_eq!(saved_sha, outgoing.sha256);
                assert_eq!(fs::read(&target).unwrap().len(), size);
                assert_eq!(sha256_file(&target).unwrap().0, outgoing.sha256);

                // 传完之后不应该再有 .part 残留
                let leftovers = fs::read_dir(store.tmp_dir())
                    .unwrap()
                    .flatten()
                    .filter(|entry| {
                        entry.path().extension().and_then(|value| value.to_str()) == Some("part")
                    })
                    .count();

                assert_eq!(leftovers, 0);
            }
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hash_mismatch_deletes_the_part_file() {
        let root = temp_dir("hash-mismatch");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let source = write_source(&root, "src.bin", 4096);
        let (sha256, size) = sha256_file(&source).unwrap();
        let outgoing = OutgoingTransfer::with_digest(
            source,
            "src.bin",
            "application/octet-stream",
            size,
            sha256,
            CHUNK_SIZE,
        );

        // 声明的 sha256 被篡改
        let mut incoming = IncomingTransfer::create(
            &store.tmp_dir(),
            outgoing.size,
            &"0".repeat(64),
            outgoing.chunks,
            CHUNK_SIZE,
        )
        .unwrap();
        let chunk = outgoing.read_chunk(0).unwrap();

        incoming.write_chunk(0, &chunk).unwrap();

        let part = incoming.part.clone();
        let error = incoming
            .finish(store.attachment_path("x.bin"))
            .unwrap_err();

        assert!(error.contains("SHA-256"), "{error}");
        assert!(!part.exists(), ".part 应该被删掉");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_out_of_order_and_oversized_chunks() {
        let root = temp_dir("bad-chunks");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let mut incoming = IncomingTransfer::create(
            &store.tmp_dir(),
            (CHUNK_SIZE as u64) + 1,
            &"0".repeat(64),
            2,
            CHUNK_SIZE,
        )
        .unwrap();

        // 序号必须从 0 开始
        assert!(incoming.write_chunk(1, &[0u8; 1]).is_err());
        // 大小必须与 offer 声明的一致
        assert!(incoming.write_chunk(0, &[0u8; 16]).is_err());
        // 超出声明的分片数
        assert!(incoming.write_chunk(9, &[0u8; 1]).is_err());

        incoming.abort();
        assert!(fs::read_dir(store.tmp_dir()).unwrap().next().is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn aborted_transfers_leave_nothing_behind() {
        let root = temp_dir("abort");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let incoming =
            IncomingTransfer::create(&store.tmp_dir(), 10, &"0".repeat(64), 1, CHUNK_SIZE).unwrap();
        let part = incoming.part.clone();

        assert!(part.exists());

        incoming.abort();

        assert!(!part.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn staging_copies_into_the_cache_and_removes_the_source() {
        let root = temp_dir("stage");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let source = write_source(&root, "pasted.png", 32);
        let staged = store.stage_copy(&source, "pasted.png").unwrap();

        assert!(!source.exists());
        assert!(staged.exists());
        assert_eq!(staged.extension().and_then(|value| value.to_str()), Some("png"));
        assert_eq!(fs::read(&staged).unwrap().len(), 32);
        // 落盘名是 UUID，不含原始名字
        assert!(!staged.file_name().unwrap().to_string_lossy().contains("pasted"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn oversized_files_are_rejected_before_reading_them() {
        assert_eq!(clamp_limit(256), DEFAULT_MAX_SIZE);
        assert_eq!(clamp_limit(4096), HARD_MAX_SIZE);
        assert_eq!(clamp_limit(0), 1024 * 1024);

        assert!(needs_confirmation(TransferKind::File, LARGE_FILE_CONFIRM_SIZE + 1));
        assert!(!needs_confirmation(TransferKind::File, LARGE_FILE_CONFIRM_SIZE));
        assert!(!needs_confirmation(TransferKind::Image, HARD_MAX_SIZE));
        assert!(!needs_confirmation(TransferKind::Voice, HARD_MAX_SIZE));
    }

    #[test]
    fn stale_parts_are_cleaned_up_but_fresh_ones_survive() {
        let root = temp_dir("stale");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let fresh = store.tmp_dir().join(format!("{}.part", Uuid::new_v4()));
        let stale = store.tmp_dir().join(format!("{}.part", Uuid::new_v4()));

        fs::write(&fresh, b"fresh").unwrap();
        fs::write(&stale, b"stale").unwrap();

        // 把 mtime 往前拨到「过期」
        let old = SystemTime::now() - STALE_PART_AGE - Duration::from_secs(60);
        let file = OpenOptions::new().write(true).open(&stale).unwrap();

        file.set_modified(old).unwrap();

        store.ensure().unwrap();

        assert!(fresh.exists());
        assert!(!stale.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    /// R41 / R44：上一次运行留下的「录完待确认」wav 只在启动时清一次，
    /// 而且只认 `voice-*.wav` —— 别把别的临时文件（`.part`、别的 wav）顺手删掉。
    #[test]
    fn orphan_recordings_are_cleaned_up_without_touching_other_files() {
        let root = temp_dir("orphan-voice");
        let store = TransferStore::new(&root);

        store.ensure().unwrap();

        let orphan = store.tmp_dir().join("voice-20260925-120000.wav");
        let other_wav = store.tmp_dir().join("note.wav");
        let part = store.tmp_dir().join(format!("{}.part", Uuid::new_v4()));

        fs::write(&orphan, b"orphan").unwrap();
        fs::write(&other_wav, b"keep").unwrap();
        fs::write(&part, b"keep").unwrap();

        store.cleanup_orphan_recordings();

        assert!(!orphan.exists());
        assert!(other_wav.exists());
        assert!(part.exists());

        fs::remove_dir_all(&root).unwrap();
    }
}

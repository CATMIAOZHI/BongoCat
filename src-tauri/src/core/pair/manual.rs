//! 配对码：不经过任何服务器的手工信令（手工码模式）。
//!
//! 背景：STUN 只回答「我自己的公网映射是什么」，它**不传送**对端的 SDP / ICE candidate。
//! 握手必须先交换「怎么找到我」的信息，所以必须有一个**双方事先都知道的会合点**——中继、
//! 局域网广播，或者**人肉转送**。这个模块是最后那条路：把「本来要发给中继的那份信令」
//! 压成一段文本，用户自己用微信发过去，另一端粘回来。
//!
//! 码的形状（`BGP1:` 前缀 + base64url 无填充）：
//!
//! ```text
//! BGP1: || base64url( nonce(24B) || XChaCha20-Poly1305( deflate( JSON ) ) )
//! ```
//!
//! - **整段码是加密的**：密钥由配对密码 HKDF 派生（见 [`CODE_INFO`]）。这一条同时解决
//!   三件事——伪造码改一下就解不开（真实性）、码里的内网 / 公网地址对微信这种第三方
//!   不可见（隐私）、别人的码（配对密码不同）解不开且报错明确。
//! - **先压后加**：SDP 高度可压缩，而 base64 会膨胀 1/3。压完能把码从 1.2~1.5K 字符
//!   降到 400~650，微信长文本被折叠 / 转成文件的风险明显变小。
//! - 解析时**吃掉所有空白**，所以微信折叠换行不影响粘贴。
//!
//! 只做编解码，不碰 WebRTC：状态机与会话组织在 `manager.rs`，设计依据见
//! `docs/pair-plan-manual-code.md`。

use std::io::{Read as _, Write as _};

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rand::Rng as _;
use serde::{Deserialize, Serialize};

use super::crypto::{self, PAIR_SECRET_BYTES};
use super::protocol::{NONCE_SIZE, now_millis};

/// 码的前缀。带版本号，将来换格式时能给出「请升级」而不是「码损坏」。
pub const CODE_PREFIX: &str = "BGP1:";
pub const CODE_VERSION: u8 = 1;
/// 派生码的加密密钥用的 HKDF info（与 `crypto.rs` 里那几条并列、互不相干）。
pub const CODE_INFO: &[u8] = b"bongocat-pair-code-v1";
/// 一段码能有多长。超过就按「粘错了」处理，不再尝试解析。
pub const MAX_CODE_LENGTH: usize = 8192;
/// 解压后的上限（解压炸弹防护）：真实 SDP 只有几 KB。
const MAX_DECODED_BYTES: usize = 64 * 1024;
/// offer 码的有效期：出码 → 对方粘贴 → 出回码 → 自己粘贴，10 分钟够用。
pub const OFFER_TTL_MS: i64 = 10 * 60 * 1000;
/// 回码的有效期。
///
/// 它**不只是**「贴得快点」的问题：粘贴方那台电脑的 ICE 计时是从它生成回码那一刻开始的
/// （手工码模式把它放宽到 10 + 120 秒，见 `p2p.rs` 的 `MANUAL_FAILED_TIMEOUT`），PC 一失败
/// 就被丢掉，那段回码也就没用了。所以这里跟那个窗口对齐：码比 PC 多活一点，让「码还能解开、
/// 但连不上」这种情况尽量少见——真发生了也只是报「没打通」。
pub const ANSWER_TTL_MS: i64 = 3 * 60 * 1000;

/// 出码方等「候选收集完成」的上限。
///
/// 这个时限不是保险起见加的：STUN 完全不响应时，底层的 gatherer 不会把没响应的 client
/// 从表里摘掉，`gathering complete` 就**永远不来**（见 `docs/pair-plan-manual-code.md`）。
/// 到点就带上有多少算多少的候选出码，好过让用户对着「正在生成」干等。
pub const CODE_GATHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 默认的公益 STUN 清单（2026-09-28 本机实测，国内可用、按实测延迟排）。
///
/// 这里只放 `stun:`：`turn:` 只能来自中继的广告（自建中继配了 coturn 时），在本地清单里
/// 接受 `turn:` 会让「填错一行就把中继的 TURN 顶掉」这种事发生。
///
/// `stun1.douyucdn.cn:3478` 两轮实测都超时，是死端口，**不要**加回来；
/// `stun.l.google.com:19302` 国内 UDP 常受干扰，只在帮助文案里当示例。
pub const DEFAULT_STUN_URLS: [&str; 6] = [
    "stun.miwifi.com:3478",
    "stun.hitv.com:3478",
    "stun.chat.bilibili.com:3478",
    "stun.douyucdn.cn:18000",
    "stun1.douyucdn.cn:18000",
    "stun.cloudflare.com:3478",
];

/// 一份清单最多几条。够了就够：每个 STUN 服务器都会给同一条 NAT 映射出一份 srflx 候选，
/// 六条已经能覆盖「某一家挂了」。超过就报错让用户自己删，不静默截断。
pub const MAX_STUN_URLS: usize = 6;

/// 缺端口时的默认端口（STUN 的标准端口）
const DEFAULT_STUN_PORT: u16 = 3478;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManualCodeKind {
    Offer,
    Answer,
}

impl ManualCodeKind {
    pub const fn ttl_ms(self) -> i64 {
        match self {
            Self::Offer => OFFER_TTL_MS,
            Self::Answer => ANSWER_TTL_MS,
        }
    }
}

/// 一段配对码的内容。字段名在**加密内层**，所以取短名省字节（压过之后差别不大，
/// 但白送的空间没理由不要）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualCode {
    #[serde(rename = "v")]
    pub version: u8,
    /// `offer` = 出码方（同时也是 offerer）；`answer` = 粘贴方。
    #[serde(rename = "k")]
    pub kind: ManualCodeKind,
    #[serde(rename = "d")]
    pub device_id: String,
    /// 一次配对的随机 id：两段码必须属于同一个会话，防止把码粘到别的会话里。
    #[serde(rename = "sid")]
    pub session_id: String,
    #[serde(rename = "t")]
    pub created_at: i64,
    /// 能力位。手工模式没有 `hello`，对端只能从这里知道我们支不支持可靠通道——
    /// 不带的话聊天 / 附件 / 语音会全废（`p2p.rs` 的 `acceptable_lane`）。
    #[serde(rename = "f", default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// 序列化后的 `RTCSessionDescription`（含类型与全部已收集的候选），与中继那条
    /// 信令的 `description` 字段是**同一个东西**，可以直接喂 `handle_signal`。
    #[serde(rename = "s")]
    pub description: String,
}

impl ManualCode {
    pub fn offer(device_id: &str, session_id: &str, features: Vec<String>, description: String) -> Self {
        Self {
            version: CODE_VERSION,
            kind: ManualCodeKind::Offer,
            device_id: device_id.to_string(),
            session_id: session_id.to_string(),
            created_at: now_millis(),
            features,
            description,
        }
    }

    pub fn answer(
        device_id: &str,
        session_id: &str,
        features: Vec<String>,
        description: String,
    ) -> Self {
        Self {
            version: CODE_VERSION,
            kind: ManualCodeKind::Answer,
            device_id: device_id.to_string(),
            session_id: session_id.to_string(),
            created_at: now_millis(),
            features,
            description,
        }
    }

    /// 这串码到什么时候过期（毫秒时间戳）。界面的倒计时用它，不用自己再算一遍 TTL。
    pub fn expires_at(&self) -> i64 {
        self.created_at + self.kind.ttl_ms()
    }
}

/// 一次新的会话 id（8 字节随机，base64url 后 11 个字符）。
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 8];

    rand::rng().fill_bytes(&mut bytes);

    URL_SAFE_NO_PAD.encode(bytes)
}

/// 码的加密密钥：`HKDF-SHA256(配对密码, info = CODE_INFO)`。
pub fn code_key(secret: &[u8; PAIR_SECRET_BYTES]) -> [u8; 32] {
    crypto::hkdf_sha256(secret, CODE_INFO)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeError {
    /// 输入是空的
    Missing,
    /// 前缀不对（或粘贴进来的根本不是码）
    Prefix,
    /// 超长
    TooLong,
    /// base64 解不开
    Base64,
    /// 内容太短
    Short,
    /// 解密失败（配对密码不一致 / 被别人改过）
    Decrypt,
    /// 压缩流坏了
    Inflate,
    /// 解压后不是合法的 JSON
    Json,
    /// 版本不认识
    Version,
    /// 过期
    Expired,
    /// 出码方生成失败（内存 / 编码器错误，实际到不了）
    Encode,
}

impl CodeError {
    /// 给用户看的一句话。**不暴露内部细节**：解密失败不区分「密码不对」与「被篡改」，
    /// 因为两者对用户是同一件事（让对方确认配对密码 / 重新出码）。
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::Missing => "配对码是空的",
            Self::Prefix => "这看起来不是配对码（开头应该是 BGP1:）",
            Self::TooLong => "配对码太长了，可能粘错了内容",
            Self::Base64 | Self::Short => "配对码不完整，复制的时候可能被截断了",
            Self::Decrypt => "配对码打不开：两边的配对密码可能不一致，或者这个码被改过",
            Self::Inflate | Self::Json => "配对码内容已损坏，请让对方重新生成",
            Self::Version => "配对码的版本不支持，请把两台电脑的 BongoCat 升级到同一版本",
            Self::Expired => "这个配对码已经过期了，请重新生成",
            Self::Encode => "生成配对码失败，请重试",
        }
    }
}

impl std::fmt::Display for CodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.user_message())
    }
}

/// 把一段码编成可以贴进聊天窗口的文本。
pub fn encode(secret: &[u8; PAIR_SECRET_BYTES], code: &ManualCode) -> Result<String, CodeError> {
    let json = serde_json::to_vec(code).map_err(|_| CodeError::Encode)?;
    let compressed = deflate(&json)?;

    let cipher = XChaCha20Poly1305::new(&Key::from(code_key(secret)));
    let mut nonce_bytes = [0u8; NONCE_SIZE];

    rand::rng().fill_bytes(&mut nonce_bytes);

    // 前缀进 AAD：换了前缀就解不开，省得「BGP2 的码被当 BGP1 解析」这种事
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce_bytes),
            Payload {
                msg: &compressed,
                aad: CODE_PREFIX.as_bytes(),
            },
        )
        .map_err(|_| CodeError::Encode)?;

    let mut body = Vec::with_capacity(NONCE_SIZE + ciphertext.len());

    body.extend_from_slice(&nonce_bytes);
    body.extend_from_slice(&ciphertext);

    let text = format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(body));

    // 字段膨胀导致码长到发不出去，属于实现问题：在这里挡住，比让用户事后发现更好
    if text.len() > MAX_CODE_LENGTH {
        return Err(CodeError::TooLong);
    }

    Ok(text)
}

/// 解开一段码。会吃掉所有空白，并校验版本与有效期。
pub fn decode(secret: &[u8; PAIR_SECRET_BYTES], text: &str) -> Result<ManualCode, CodeError> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();

    if compact.is_empty() {
        return Err(CodeError::Missing);
    }

    if compact.len() > MAX_CODE_LENGTH {
        return Err(CodeError::TooLong);
    }

    let body = strip_prefix(&compact)?;
    let raw = decode_base64(body)?;

    if raw.len() < NONCE_SIZE + 16 {
        return Err(CodeError::Short);
    }

    let (nonce_bytes, ciphertext) = raw.split_at(NONCE_SIZE);
    let nonce: [u8; NONCE_SIZE] = nonce_bytes.try_into().map_err(|_| CodeError::Short)?;
    let plaintext = XChaCha20Poly1305::new(&Key::from(code_key(secret)))
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: CODE_PREFIX.as_bytes(),
            },
        )
        .map_err(|_| CodeError::Decrypt)?;

    let json = inflate(&plaintext)?;
    let code: ManualCode = serde_json::from_slice(&json).map_err(|_| CodeError::Json)?;

    if code.version != CODE_VERSION {
        return Err(CodeError::Version);
    }

    // 时钟不同步不做限制：只挡「太旧」。时间在未来（对方时钟快了）照收。
    if now_millis() - code.created_at > code.kind.ttl_ms() {
        return Err(CodeError::Expired);
    }

    Ok(code)
}

/// 去掉前缀，大小写不敏感（有的客户端 / 输入法会把它改成小写）。
///
/// 用 `str::get` 按字节切片而不是 `split_at`：粘贴内容**不一定是 ASCII**（用户可能直接
/// 粘一段中文过来），而 `split_at(5)` 落在多字节字符中间会 panic —— release 档是
/// `panic = "abort"`，那样整个进程就没了。`get` 越界或不在字符边界上都只返回 `None`。
fn strip_prefix(text: &str) -> Result<&str, CodeError> {
    let head = text.get(..CODE_PREFIX.len()).ok_or(CodeError::Prefix)?;

    if head.eq_ignore_ascii_case(CODE_PREFIX) {
        // 前缀是纯 ASCII，能对上也说明这几个字节都是完整的字符
        text.get(CODE_PREFIX.len()..).ok_or(CodeError::Prefix)
    } else {
        Err(CodeError::Prefix)
    }
}

/// 无填充的 base64url；顺便兼容带 `=` 的写法（有工具会补上）。
fn decode_base64(body: &str) -> Result<Vec<u8>, CodeError> {
    URL_SAFE_NO_PAD
        .decode(body)
        .or_else(|_| URL_SAFE.decode(body))
        .map_err(|_| CodeError::Base64)
}

fn deflate(input: &[u8]) -> Result<Vec<u8>, CodeError> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());

    encoder.write_all(input).map_err(|_| CodeError::Encode)?;

    encoder.finish().map_err(|_| CodeError::Encode)
}

fn inflate(input: &[u8]) -> Result<Vec<u8>, CodeError> {
    let mut out = Vec::new();
    let mut decoder = DeflateDecoder::new(input).take(MAX_DECODED_BYTES as u64 + 1);

    decoder.read_to_end(&mut out).map_err(|_| CodeError::Inflate)?;

    if out.len() > MAX_DECODED_BYTES {
        return Err(CodeError::Inflate);
    }

    Ok(out)
}

/// 用户填的 STUN 清单解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StunList {
    /// 归一化后的地址（`stun:` 开头、去重、保序）。`errors` 非空时**不要保存**，
    /// 这里的内容只用来给界面做预览。
    pub urls: Vec<String>,
    /// 逐行的问题（「第 2 行：…」）。非空就要挡下保存——静默丢掉一行会让用户
    /// 以为「填了六个」，实际只有五个。
    pub errors: Vec<String>,
    /// 输入是空的：调用方据此回落到 [`DEFAULT_STUN_URLS`]
    pub empty: bool,
}

impl StunList {
    /// 真正要用的清单：填了就按填的，没填（或只填了空行/注释）就用默认的。
    pub fn effective(&self) -> Vec<String> {
        if self.empty || self.urls.is_empty() {
            DEFAULT_STUN_URLS.iter().map(|url| url.to_string()).collect()
        } else {
            self.urls.clone()
        }
    }
}

/// 解析用户填的 STUN 清单。一行一条；`#` 开头当注释；空行跳过。
///
/// 接受的写法（都归一化成 `stun:host:port`）：
///
/// - `stun.miwifi.com`（补默认端口）
/// - `stun.miwifi.com:3478`
/// - `stun:stun.miwifi.com:3478`
///
/// 明确拒绝的：`turn:` / `turns:`（那是中继才能给的东西）、`stuns:`（TLS 的 STUN 不在这条
/// 路上）、IPv6 字面量（v1 不做双栈，绑了只会多一堆解析失败的日志）、缺少主机名、
/// 端口不是 1~65535 的数字。
pub fn parse_stun_urls(text: &str) -> StunList {
    let mut urls: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut saw_any = false;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        saw_any = true;

        match normalize_stun_url(line) {
            Ok(url) => {
                if urls.iter().any(|known| known.eq_ignore_ascii_case(&url)) {
                    continue;
                }

                urls.push(url);
            }
            Err(reason) => errors.push(format!("第 {} 行：{reason}", index + 1)),
        }
    }

    if urls.len() > MAX_STUN_URLS {
        errors.push(format!("最多填 {MAX_STUN_URLS} 条，请删掉多余的"));
        urls.truncate(MAX_STUN_URLS);
    }

    StunList {
        urls,
        errors,
        empty: !saw_any,
    }
}

/// 一行地址的归一化与校验，见 [`parse_stun_urls`]。
fn normalize_stun_url(line: &str) -> Result<String, String> {
    let lower = line.to_ascii_lowercase();

    if lower.starts_with("turn:") || lower.starts_with("turns:") {
        return Err("这里只能填 STUN；TURN 由服务器（中继）提供，填了也不会用".to_string());
    }

    if lower.starts_with("stuns:") || lower.starts_with("stun://") {
        return Err("只支持 stun:，不支持 stuns: 或 stun://".to_string());
    }

    let body = if lower.starts_with("stun:") {
        &line["stun:".len()..]
    } else {
        line
    };
    let body = body.trim();

    if body.is_empty() {
        return Err("缺少服务器地址".to_string());
    }

    if body.contains('[') || body.contains(']') {
        return Err("暂不支持 IPv6 地址".to_string());
    }

    if body.contains('/') || body.contains(char::is_whitespace) {
        return Err("地址里不能有空格或斜杠".to_string());
    }

    let (host, port) = match body.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port
                .parse()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("端口 `{port}` 不对，应该是 1~65535 的数字"))?;

            (host, port)
        }
        None => (body, DEFAULT_STUN_PORT),
    };

    if host.is_empty() {
        return Err("缺少主机名".to_string());
    }

    // 主机名只留「字母数字点横杠」：挡住 `?transport=udp`、`user:pass@host` 这类
    // 只对 TURN 有意义的写法，也免得把一个奇怪的字符串喂给 ICE 栈
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err("主机名里有不支持的字符".to_string());
    }

    Ok(format!("stun:{host}:{port}"))
}

/// 把清单变成 ICE 条目形状（一个 `IceServer` 装一条 URL）。
///
/// **只给手工码那条路用**：中继模式的 ICE 条目一律来自 `server.welcome` 的广告，两边不
/// 合并——`turn:` 的唯一来源必须是中继，本地清单里混进一条就会把真正的 TURN 顶掉。
pub fn stun_ice_servers(urls: &[String]) -> Vec<super::protocol::IceServer> {
    urls.iter()
        .map(|url| super::protocol::IceServer {
            urls: vec![url.clone()],
            username: String::new(),
            credential: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; PAIR_SECRET_BYTES] = [7u8; PAIR_SECRET_BYTES];
    const OTHER: [u8; PAIR_SECRET_BYTES] = [9u8; PAIR_SECRET_BYTES];
    const PRIVATE_IP: &str = "192.168.31.101";

    /// 一份形状接近真实的 data-channel SDP（含两个 host 候选）。
    fn sample_sdp() -> String {
        let mut sdp = String::from(
            "v=0\r\n\
             o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0\r\n\
             a=extmap-allow-mixed\r\n\
             a=msid-semantic: WMS\r\n\
             m=application 51610 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             c=IN IP4 192.168.31.101\r\n\
             a=candidate:1234567890 1 udp 2122260223 192.168.31.101 51610 typ host generation 0 network-id 1 network-cost 10\r\n\
             a=candidate:1234567891 1 udp 1686052607 112.18.213.255 32775 typ srflx raddr 192.168.31.101 rport 51610 generation 0 network-id 1 network-cost 10\r\n\
             a=ice-ufrag:xYz1\r\n\
             a=ice-pwd:9fJ2kQ0lMn7pQr4tUvWx8yZa\r\n\
             a=ice-options:trickle\r\n\
             a=fingerprint:sha-256 6B:8B:2F:1C:0A:5D:E7:33:44:21:9A:88:C0:1E:2B:7D:55:63:41:02:AF:9C:DE:77:31:20:8E:6C:5A:91:B3:04\r\n\
             a=setup:actpass\r\n\
             a=mid:0\r\n\
             a=sctp-port:5000\r\n\
             a=max-message-size:262144\r\n",
        );

        sdp.push_str("a=end-of-candidates\r\n");

        sdp
    }

    fn offer_code() -> ManualCode {
        ManualCode::offer(
            "device-a",
            "sid-1234",
            vec!["reliable-channel".into()],
            sample_sdp(),
        )
    }

    #[test]
    fn an_offer_code_round_trips() {
        let text = encode(&SECRET, &offer_code()).unwrap();
        let code = decode(&SECRET, &text).unwrap();

        assert!(text.starts_with(CODE_PREFIX));
        assert_eq!(code.version, CODE_VERSION);
        assert_eq!(code.kind, ManualCodeKind::Offer);
        assert_eq!(code.device_id, "device-a");
        assert_eq!(code.session_id, "sid-1234");
        assert_eq!(code.features, vec!["reliable-channel".to_string()]);
        assert_eq!(code.description, sample_sdp());
    }

    #[test]
    fn an_answer_code_round_trips() {
        let answer = ManualCode::answer(
            "device-b",
            "sid-1234",
            vec!["reliable-channel".into()],
            sample_sdp(),
        );
        let code = decode(&SECRET, &encode(&SECRET, &answer).unwrap()).unwrap();

        assert_eq!(code.kind, ManualCodeKind::Answer);
        assert_eq!(code.device_id, "device-b");
    }

    #[test]
    fn the_code_is_short_enough_to_type_into_a_chat() {
        let text = encode(&SECRET, &offer_code()).unwrap();

        // 压过之后的量级：几百字符。这里留足余量，只挡住「膨胀到发不出去」
        assert!(text.len() < 1024, "配对码太长了: {} 字符", text.len());
    }

    #[test]
    fn the_code_does_not_leak_addresses_or_sdp() {
        let text = encode(&SECRET, &offer_code()).unwrap();

        assert!(!text.contains(PRIVATE_IP));
        assert!(!text.contains("v=0"));
        assert!(!text.contains("candidate"));
        assert!(!text.contains("device-a"));
    }

    #[test]
    fn another_pair_cannot_open_the_code() {
        let text = encode(&SECRET, &offer_code()).unwrap();

        assert_eq!(decode(&OTHER, &text).unwrap_err(), CodeError::Decrypt);
    }

    #[test]
    fn a_tampered_code_is_rejected() {
        let text = encode(&SECRET, &offer_code()).unwrap();
        let mut chars: Vec<char> = text.chars().collect();
        // 改**倒数第 6 位**，不是最后一位。
        //
        // 码的载荷是压缩后再加密的，长度会随压缩结果变化，所以最后一位有相当一部分概率
        // 只落在 base64 的填充位上——改它解码出来的字节一模一样，AEAD 当然不失败，这条
        // 用例就会随机变红（实测约 1/6）。往回收 6 个字符一定落在密文里（后面还有 16 字节
        // 的 tag），改动一定被 AEAD 抓出来。
        let target = chars.len() - 6;
        chars[target] = if chars[target] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();

        assert_eq!(decode(&SECRET, &tampered).unwrap_err(), CodeError::Decrypt);
    }

    #[test]
    fn whitespace_and_prefix_case_are_tolerated() {
        let text = encode(&SECRET, &offer_code()).unwrap();
        let folded = format!("  bgp1:{}\n  \t", &text[CODE_PREFIX.len()..]);
        let code = decode(&SECRET, &folded).unwrap();

        assert_eq!(code.session_id, "sid-1234");
    }

    #[test]
    fn padding_is_tolerated() {
        let body = URL_SAFE.encode(
            URL_SAFE_NO_PAD
                .decode(&encode(&SECRET, &offer_code()).unwrap()[CODE_PREFIX.len()..])
                .unwrap(),
        );

        assert_eq!(
            decode(&SECRET, &format!("{CODE_PREFIX}{body}"))
                .unwrap()
                .device_id,
            "device-a"
        );
    }

    #[test]
    fn empty_garbage_and_oversized_input_are_rejected() {
        assert_eq!(decode(&SECRET, "   \n ").unwrap_err(), CodeError::Missing);
        assert_eq!(decode(&SECRET, "hello").unwrap_err(), CodeError::Prefix);
        assert_eq!(
            decode(&SECRET, &format!("{CODE_PREFIX}!!!!").replace(' ', "")).unwrap_err(),
            CodeError::Base64
        );
        assert_eq!(
            decode(&SECRET, &"A".repeat(MAX_CODE_LENGTH + 1)).unwrap_err(),
            CodeError::TooLong
        );
    }

    /// 粘贴内容不一定是 ASCII。以前 `strip_prefix` 用 `split_at(5)` 按字节切，粘一段中文
    /// 进来就会 panic（release 档 `panic = "abort"` = 整个进程没了），所以这条要钉住。
    #[test]
    fn non_ascii_pastes_are_rejected_without_panicking() {
        for text in ["你好", "你好世界", "配对码：BGP1", "👍👍👍", "BGP1：中文"] {
            assert_eq!(decode(&SECRET, text).unwrap_err(), CodeError::Prefix);
        }

        // 前缀对了、后面是中文：不该 panic，按 base64 失败收场
        assert_eq!(
            decode(&SECRET, &format!("{CODE_PREFIX}你好")).unwrap_err(),
            CodeError::Base64
        );
    }

    #[test]
    fn an_expired_code_is_rejected() {
        let mut offer = offer_code();

        offer.created_at = now_millis() - OFFER_TTL_MS - 1;

        assert_eq!(
            decode(&SECRET, &encode(&SECRET, &offer).unwrap()).unwrap_err(),
            CodeError::Expired
        );

        // 回码的有效期更短：刚过 5 分钟就不认了
        let mut answer = ManualCode::answer("device-b", "sid-1234", Vec::new(), sample_sdp());

        answer.created_at = now_millis() - ANSWER_TTL_MS - 1;

        assert_eq!(
            decode(&SECRET, &encode(&SECRET, &answer).unwrap()).unwrap_err(),
            CodeError::Expired
        );
    }

    #[test]
    fn a_future_timestamp_still_decodes() {
        let mut offer = offer_code();

        offer.created_at = now_millis() + 60_000;

        assert!(decode(&SECRET, &encode(&SECRET, &offer).unwrap()).is_ok());
    }

    #[test]
    fn an_unknown_version_is_rejected() {
        let mut offer = offer_code();

        offer.version = CODE_VERSION + 1;

        assert_eq!(
            decode(&SECRET, &encode(&SECRET, &offer).unwrap()).unwrap_err(),
            CodeError::Version
        );
    }

    #[test]
    fn expires_at_follows_the_ttl_of_each_kind() {
        let mut offer = offer_code();

        offer.created_at = 1_000;

        assert_eq!(offer.expires_at(), 1_000 + OFFER_TTL_MS);

        let mut answer = ManualCode::answer("device-b", "sid-1234", Vec::new(), sample_sdp());

        answer.created_at = 1_000;

        // 回码的有效期更短
        assert_eq!(answer.expires_at(), 1_000 + ANSWER_TTL_MS);
        assert!(ANSWER_TTL_MS < OFFER_TTL_MS);
    }

    #[test]
    fn session_ids_are_unique_and_short() {
        let first = new_session_id();
        let second = new_session_id();

        assert_ne!(first, second);
        assert_eq!(first.len(), 11);
    }

    #[test]
    fn every_error_has_a_message() {
        for error in [
            CodeError::Missing,
            CodeError::Prefix,
            CodeError::TooLong,
            CodeError::Base64,
            CodeError::Short,
            CodeError::Decrypt,
            CodeError::Inflate,
            CodeError::Json,
            CodeError::Version,
            CodeError::Expired,
            CodeError::Encode,
        ] {
            assert!(!error.user_message().is_empty());
        }
    }

    #[test]
    fn an_empty_stun_list_falls_back_to_the_defaults() {
        let defaults: Vec<String> = DEFAULT_STUN_URLS.iter().map(|url| url.to_string()).collect();
        let parsed = parse_stun_urls("  \n# 注释\n\n");

        assert!(parsed.empty);
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.effective(), defaults);

        // 只填了空白与注释也算「没填」：默认清单必须真的起作用
        assert_eq!(parse_stun_urls("").effective(), defaults);
    }

    #[test]
    fn stun_urls_are_normalized_and_deduplicated() {
        let parsed = parse_stun_urls(
            "stun.miwifi.com\r\n\
             stun.miwifi.com:3478\n\
             STUN:hitv.com\n\
             stun:stun.douyucdn.cn:18000",
        );

        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        // 前两条是同一个地址（只差默认端口），第三、四条归一到 stun: 开头
        assert_eq!(
            parsed.urls,
            vec![
                "stun:stun.miwifi.com:3478".to_string(),
                "stun:hitv.com:3478".to_string(),
                "stun:stun.douyucdn.cn:18000".to_string(),
            ]
        );
    }

    #[test]
    fn a_turn_url_is_rejected_with_the_line_number() {
        let parsed = parse_stun_urls("stun.miwifi.com\nturn:cat.example.com:3478");

        assert_eq!(parsed.urls, vec!["stun:stun.miwifi.com:3478".to_string()]);
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].starts_with("第 2 行"), "{:?}", parsed.errors);
    }

    #[test]
    fn bad_lines_are_reported_instead_of_silently_dropped() {
        let parsed = parse_stun_urls(
            "stuns:cat.example.com:3478\n\
             [::1]:3478\n\
             stun.miwifi.com:0\n\
             stun.miwifi.com:70000\n\
             :3478\n\
             stun.miwifi.com?transport=udp",
        );

        assert!(parsed.urls.is_empty());
        assert_eq!(parsed.errors.len(), 6, "{:?}", parsed.errors);
    }

    #[test]
    fn more_than_the_cap_is_an_error_and_keeps_the_first_six() {
        let parsed = parse_stun_urls(
            "a.example.com\nb.example.com\nc.example.com\nd.example.com\n\
             e.example.com\nf.example.com\ng.example.com",
        );

        assert_eq!(parsed.urls.len(), MAX_STUN_URLS);
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("最多"), "{:?}", parsed.errors);
    }

    #[test]
    fn every_default_stun_url_is_accepted_by_its_own_parser() {
        let parsed = parse_stun_urls(&DEFAULT_STUN_URLS.join("\n"));

        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.urls.len(), DEFAULT_STUN_URLS.len());
        assert_eq!(stun_ice_servers(&parsed.urls)[0].urls, vec!["stun:stun.miwifi.com:3478".to_string()]);
    }
}

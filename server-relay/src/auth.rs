//! Pair Secret 的派生与鉴权工具。
//!
//! 与 `src-tauri/src/core/pair/crypto.rs`、`server-cloudflare/scripts/generate-pair.mjs`
//! 共用同一组参数（R17）：HKDF-SHA256、salt 为空、输出 32 字节。
//!
//! ```text
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-auth-v1")--> PAIR_AUTH_TOKEN
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-e2ee-v1")--> E2EE_ROOT_KEY
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-room-v1")--> ROOM_ID
//! ```
//!
//! 多会话（§4 / §7）下中继**不再配置任何 secret**：`ROOM_ID` 与 `AUTH_TOKEN` 都由
//! 客户端在连接时给出。中继只保存 `SHA256(AUTH_TOKEN)`（见 [`auth_verifier`]），
//! 用它判断「后面进来的人是不是同一对」——它永远拿不到 Pair Secret 与 E2EE 根密钥。

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use hkdf::Hkdf;
use sha2::{Digest as _, Sha256};

pub const AUTH_INFO: &[u8] = b"bongocat-pair-auth-v1";
pub const E2EE_INFO: &[u8] = b"bongocat-pair-e2ee-v1";
pub const ROOM_INFO: &[u8] = b"bongocat-pair-room-v1";
pub const SERVER_INFO: &[u8] = b"bongocat-pair-server-v1";
pub const PAIR_SECRET_BYTES: usize = 32;

/// HKDF-SHA256，salt 为空（RFC 5869 的「无 salt」与「32 字节零 salt」等价，
/// 因此与 WebCrypto 里 `salt: new Uint8Array(0)` 的结果一致）。
pub fn hkdf_sha256(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(None, ikm);
    let mut okm = [0u8; 32];

    hkdf.expand(info, &mut okm)
        .expect("32 字节对 HKDF-SHA256 来说永远是合法的输出长度");

    okm
}

/// 派生鉴权 token（base64url 无填充，43 字符）
pub fn derive_auth_token(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(hkdf_sha256(secret, AUTH_INFO))
}

/// 派生 E2EE 根密钥（32 原始字节）
#[allow(dead_code)]
pub fn derive_root_key(secret: &[u8; PAIR_SECRET_BYTES]) -> [u8; 32] {
    hkdf_sha256(secret, E2EE_INFO)
}

/// 派生多会话分组用的 `ROOM_ID`（§18）。中继自己不用它分组（分组键由客户端给出），
/// 保留这份实现是为了 `generate-pair` 能算出「这个密钥属于哪个会话」的指纹，
/// 并用**独立实现的固定向量**守住两端派生不漂移。
pub fn derive_room_id(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(hkdf_sha256(secret, ROOM_INFO))
}

/// 从「服务器密码」派生客户端升级头里要带的凭据（R36，base64url 无填充，43 字符）。
///
/// 服务器密码是**部署者**在服务器上自己定的文本（不是 32 字节的 Pair Secret），所以
/// 这里对任意长度的文本做同一套 HKDF：客户端填什么，中继就用同样的方式算一遍。
/// 走 HKDF 而不是直接拿密码当 token，是为了让「密码」与「线上凭据」不是同一个值
/// ——中继只保存凭据的摘要，日志里也不会出现密码本身。
pub fn derive_server_token(password: &str) -> String {
    URL_SAFE_NO_PAD.encode(hkdf_sha256(password.trim().as_bytes(), SERVER_INFO))
}

/// 服务器密码的不可逆 verifier：`SHA256(derive_server_token(password))`（与 Room 同一套纪律）。
///
/// 中继启动时算一次，之后**不再保留密码原文**；每个连接按同样方式算一份，做恒定时间比较。
pub fn server_verifier(password: &str) -> [u8; 32] {
    auth_verifier(&derive_server_token(password))
}

/// Room 的不可逆 verifier：`SHA256(AUTH_TOKEN)`（§7）。
///
/// 中继**不保存明文 token**：创建 Room 时算一次，之后每个连接都按同样方式算一份，
/// 与 Room 里存的做恒定时间比较（见 [`constant_time_eq`]）。
pub fn auth_verifier(auth_token: &str) -> [u8; 32] {
    Sha256::digest(auth_token.as_bytes()).into()
}

/// Room 的日志指纹（§29）：`SHA256(ROOM_ID)` 的前 8 字节，小写 hex。
///
/// 日志里**绝不打印完整的 `ROOM_ID`**——它是「谁是同一对」的直接证据，写进日志就等于
/// 把分组关系泄露给了任何能看到日志的人。指纹够用来定位「同一个会话的几行日志」，
/// 而且不可逆。
pub fn room_fingerprint(room_id: &str) -> String {
    let digest = Sha256::digest(room_id.as_bytes());

    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Pair Secret 的文本形态
pub fn encode_pair_secret(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(secret)
}

/// 解析用户填写的 Pair Secret（base64url，32 字节）
pub fn decode_pair_secret(text: &str) -> Result<[u8; PAIR_SECRET_BYTES], String> {
    let trimmed = text.trim();

    if trimmed.is_empty() {
        return Err("Pair Secret 不能为空".into());
    }

    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(trimmed))
        .map_err(|_| "Pair Secret 不是合法的 base64url 文本".to_string())?;

    let length = bytes.len();

    bytes
        .try_into()
        .map_err(|_| format!("Pair Secret 应为 {PAIR_SECRET_BYTES} 字节，实际 {length} 字节"))
}

/// 双方核对是否填了同一个 secret（sha256 前 16 位，两两空格分组）
pub fn fingerprint(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    let digest = Sha256::digest(secret);

    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 恒定时间比较，避免通过响应时间推断 token。
///
/// 长度不同时直接返回 false——长度本身不是秘密（token 永远是 43 字符）。
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;

    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }

    diff == 0
}

/// 解析 `Authorization` 头里的 bearer token。
///
/// 必须整串匹配：`Bearer <token>` 之外不接受任何多余内容（`split(' ')` 会放过
/// `Bearer x junk`，这与 CF 版的 `^Bearer\s+(\S+)$` 等价）。
pub fn bearer_token(header: Option<&str>) -> String {
    let Some(raw) = header else {
        return String::new();
    };

    let trimmed = raw.trim();
    let Some(rest) = trimmed.get("Bearer".len()..) else {
        return String::new();
    };

    if !trimmed
        .get(.."Bearer".len())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Bearer"))
    {
        return String::new();
    }

    // 至少要有一个空白分隔（`Bearer` 紧贴 token 是非法格式）
    let after_space = rest.trim_start_matches(char::is_whitespace);

    if after_space.len() == rest.len() || after_space.is_empty() {
        return String::new();
    }

    // token 内部不允许再出现空白
    if after_space.chars().any(char::is_whitespace) {
        return String::new();
    }

    after_space.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨语言向量：与 `server-cloudflare/test/hkdf-vector.spec.ts` 和
    /// `src-tauri/src/core/pair/crypto.rs` 里的断言必须是同一组值。
    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

    fn secret() -> [u8; PAIR_SECRET_BYTES] {
        decode_pair_secret(SECRET).unwrap()
    }

    #[test]
    fn derives_the_cross_language_vector() {
        assert_eq!(
            derive_auth_token(&secret()),
            "JSbVwoGO9GK5T58VINVkS9hNQlZMr0IYgl2_0UhEM5w"
        );
        assert_eq!(
            URL_SAFE_NO_PAD.encode(derive_root_key(&secret())),
            "am2bbPWK0R-fwuEky_8ZcFXCysV2gSD_UV6ONiWMsQk"
        );
        // 与 src-tauri/src/core/pair/crypto.rs 的 room_id_matches_the_fixed_vector
        // 必须是同一个值：两侧各写一遍，任何一侧改坏都会被对方抓住（§18）
        assert_eq!(
            derive_room_id(&secret()),
            "r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M"
        );
        assert_eq!(fingerprint(&secret()), "63 0d cd 29 66 c4 33 66");
        assert_eq!(encode_pair_secret(&secret()), SECRET);
    }

    #[test]
    fn room_verifiers_are_stable_and_do_not_leak_the_token() {
        let verifier = auth_verifier("test-token");

        assert_eq!(verifier, auth_verifier("test-token"));
        assert_ne!(verifier, auth_verifier("test-token2"));
        // verifier 里不允许出现 token 本身（哪怕只是首字节）
        assert_ne!(verifier[0], b't');

        // 日志指纹：固定长度、十六进制、与 Room ID 文本不同
        let fingerprint = room_fingerprint("r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M");

        assert_eq!(fingerprint.len(), 16);
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(fingerprint, "r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M");
        assert_eq!(
            room_fingerprint("r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M"),
            fingerprint
        );
    }

    /// R36：服务器密码的派生必须与 `src-tauri/src/core/pair/crypto.rs` 逐字节一致
    /// ——两个 crate 各写一遍同一个向量，任何一侧漂移都会在 `cargo test` 里露出来。
    /// 中文密码那一条同时守住「按 UTF-8 字节派生」，不是按 ASCII。
    #[test]
    fn derives_the_server_token_vector() {
        assert_eq!(
            derive_server_token("bongo-server-password"),
            "qi0Bz36jIJ_OpjN86BJihPxefEIuxps8XLYTOBlmEmc"
        );
        assert_eq!(
            derive_server_token("长密码测试-服务器密码"),
            "Q_HFVsHDdAD1cs13Z806GPaJU9lye7ENW3eKwTEThyU"
        );
        // 前后空白是复制粘贴的常态，不参与派生
        assert_eq!(
            derive_server_token("  bongo-server-password\n"),
            derive_server_token("bongo-server-password")
        );
    }

    /// 中继唯一保存下来的那份摘要：它必须是个**稳定**的固定向量（不是「不含密码」这种
    /// 没有意义的性质——SHA256 的输出本来就没有这种约束）。
    #[test]
    fn the_server_verifier_is_a_stable_digest() {
        let verifier = server_verifier("bongo-server-password");

        assert_eq!(verifier, server_verifier("bongo-server-password"));
        assert_ne!(verifier, server_verifier("bongo-server-password2"));

        // 摘要本身也钉一个固定向量：它是中继唯一保存下来的东西，
        // 这里漂移一次，所有已经部署好的服务器都会在升级后一夜之间连不上
        assert_eq!(
            verifier,
            [
                0x18, 0x5a, 0xe7, 0x70, 0x55, 0x9c, 0x80, 0xd2, 0x5f, 0x3b, 0x27, 0x59, 0x91, 0xc7,
                0x36, 0xfa, 0x6d, 0x0c, 0x43, 0x4f, 0x68, 0x85, 0x1b, 0xf6, 0x06, 0xc3, 0x98, 0xd6,
                0xf0, 0x97, 0x75, 0x9a
            ]
        );
    }

    #[test]
    fn rejects_malformed_secrets() {
        assert!(decode_pair_secret("").is_err());
        assert!(decode_pair_secret("   ").is_err());
        assert!(decode_pair_secret("not base64url !!").is_err());
        // 16 字节：合法 base64url，但长度不对
        assert!(decode_pair_secret("AAECAwQFBgcICQoLDA0ODw").is_err());
    }

    #[test]
    fn bearer_parsing_is_strict_but_case_insensitive() {
        assert_eq!(bearer_token(Some("Bearer abc")), "abc");
        assert_eq!(bearer_token(Some("  bearer   abc  ")), "abc");
        assert_eq!(bearer_token(Some("BEARER\tabc")), "abc");
        assert_eq!(bearer_token(Some("Bearer x junk")), "");
        assert_eq!(bearer_token(Some("Basic abc")), "");
        assert_eq!(bearer_token(Some("Bearer")), "");
        assert_eq!(bearer_token(Some("Bearerabc")), "");
        assert_eq!(bearer_token(None), "");
    }

    #[test]
    fn constant_time_comparison_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}

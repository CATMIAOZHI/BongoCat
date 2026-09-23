//! Pair Secret 的派生与鉴权工具。
//!
//! 与 `src-tauri/src/core/pair/crypto.rs`、`server-cloudflare/scripts/generate-pair.mjs`
//! 共用同一组参数（R17）：HKDF-SHA256、salt 为空、输出 32 字节。
//!
//! ```text
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-auth-v1")--> PAIR_AUTH_TOKEN
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-e2ee-v1")--> E2EE_ROOT_KEY
//! ```

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use hkdf::Hkdf;
use sha2::{Digest as _, Sha256};

pub const AUTH_INFO: &[u8] = b"bongocat-pair-auth-v1";
pub const E2EE_INFO: &[u8] = b"bongocat-pair-e2ee-v1";
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
        assert_eq!(fingerprint(&secret()), "63 0d cd 29 66 c4 33 66");
        assert_eq!(encode_pair_secret(&secret()), SECRET);
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

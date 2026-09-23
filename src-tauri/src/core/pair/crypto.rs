//! Pair Secret 的派生与端到端加密。
//!
//! 约定（必须与 `server-cloudflare/scripts/generate-pair.mjs` 完全一致）：
//!
//! ```text
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-auth-v1")--> PAIR_AUTH_TOKEN
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-e2ee-v1")--> E2EE_ROOT_KEY
//! ```
//!
//! 每个应用帧：`header(14B) || nonce(24B) || XChaCha20-Poly1305(ciphertext + tag)`，
//! 其中 header 作为 AEAD 的 associated data 参与认证。

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand::Rng as _;
use sha2::Sha256;

use super::protocol::{FRAME_HEADER_SIZE, FrameHeader, NONCE_SIZE};

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

pub fn derive_auth_token(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(hkdf_sha256(secret, AUTH_INFO))
}

pub fn derive_root_key(secret: &[u8; PAIR_SECRET_BYTES]) -> [u8; 32] {
    hkdf_sha256(secret, E2EE_INFO)
}

/// 解析用户粘贴的 Pair Secret（base64url，32 字节）
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

pub struct PairCipher {
    cipher: XChaCha20Poly1305,
}

impl PairCipher {
    pub fn new(root_key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(&Key::from(*root_key)),
        }
    }

    /// 输出完整应用帧：header || nonce || ciphertext + tag
    pub fn seal(&self, header: &FrameHeader, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let header_bytes = header.encode();
        let mut nonce_bytes = [0u8; NONCE_SIZE];

        rand::rng().fill_bytes(&mut nonce_bytes);

        let nonce = XNonce::from(nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &header_bytes,
                },
            )
            .map_err(|_| "加密失败".to_string())?;

        let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + NONCE_SIZE + ciphertext.len());

        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(&nonce_bytes);
        frame.extend_from_slice(&ciphertext);

        Ok(frame)
    }

    /// 解析并解密应用帧，返回帧头与明文
    pub fn open(&self, frame: &[u8]) -> Result<(FrameHeader, Vec<u8>), String> {
        if frame.len() < FRAME_HEADER_SIZE + NONCE_SIZE {
            return Err("帧长度不足以包含帧头与 nonce".into());
        }

        let header_bytes = &frame[..FRAME_HEADER_SIZE];
        let header = FrameHeader::decode(header_bytes).ok_or_else(|| "未知的帧类型".to_string())?;

        let nonce_bytes: [u8; NONCE_SIZE] = frame[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + NONCE_SIZE]
            .try_into()
            .map_err(|_| "nonce 长度错误".to_string())?;
        let nonce = XNonce::from(nonce_bytes);

        let plaintext = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &frame[FRAME_HEADER_SIZE + NONCE_SIZE..],
                    aad: header_bytes,
                },
            )
            .map_err(|_| "解密失败".to_string())?;

        Ok((header, plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pair::protocol::FrameKind;

    fn secret(bytes: [u8; PAIR_SECRET_BYTES]) -> [u8; PAIR_SECRET_BYTES] {
        bytes
    }

    #[test]
    fn same_secret_can_decrypt() {
        let cipher = PairCipher::new(&derive_root_key(&secret([7u8; 32])));
        let header = FrameHeader::new(FrameKind::Chat, 1);

        let frame = cipher.seal(&header, b"hello").unwrap();
        let (decoded_header, plaintext) = cipher.open(&frame).unwrap();

        assert_eq!(decoded_header, header);
        assert_eq!(plaintext, b"hello".to_vec());
    }

    #[test]
    fn different_secret_fails() {
        let sender = PairCipher::new(&derive_root_key(&secret([1u8; 32])));
        let receiver = PairCipher::new(&derive_root_key(&secret([2u8; 32])));

        let frame = sender
            .seal(&FrameHeader::new(FrameKind::Chat, 1), b"secret")
            .unwrap();

        assert!(receiver.open(&frame).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let cipher = PairCipher::new(&derive_root_key(&secret([3u8; 32])));
        let mut frame = cipher
            .seal(&FrameHeader::new(FrameKind::Chat, 9), b"tamper me")
            .unwrap();

        let last = frame.len() - 1;
        frame[last] ^= 0x01;

        assert!(cipher.open(&frame).is_err());
    }

    /// 帧头是明文但参与认证：改 kind 必须导致解密失败，否则中继可以改 kind 绕开限流
    #[test]
    fn tampered_header_fails() {
        let cipher = PairCipher::new(&derive_root_key(&secret([4u8; 32])));
        let mut frame = cipher
            .seal(&FrameHeader::new(FrameKind::PetState, 5), b"state")
            .unwrap();

        frame[0] = FrameKind::TransferChunk.as_byte();

        assert!(cipher.open(&frame).is_err());
    }

    #[test]
    fn each_frame_uses_a_fresh_nonce() {
        let cipher = PairCipher::new(&derive_root_key(&secret([5u8; 32])));
        let header = FrameHeader::new(FrameKind::Ping, 1);

        let first = cipher.seal(&header, b"same").unwrap();
        let second = cipher.seal(&header, b"same").unwrap();

        assert_ne!(first, second);

        let nonce_start = FRAME_HEADER_SIZE;
        let nonce_end = FRAME_HEADER_SIZE + NONCE_SIZE;

        assert_ne!(first[nonce_start..nonce_end], second[nonce_start..nonce_end]);
    }

    #[test]
    fn rejects_short_frames() {
        let cipher = PairCipher::new(&derive_root_key(&secret([6u8; 32])));

        assert!(cipher.open(&[0u8; FRAME_HEADER_SIZE + NONCE_SIZE - 1]).is_err());
    }

    #[test]
    fn pair_secret_decodes_with_and_without_padding() {
        let raw = [9u8; PAIR_SECRET_BYTES];

        assert_eq!(
            decode_pair_secret(&URL_SAFE_NO_PAD.encode(raw)).unwrap(),
            raw
        );
        assert_eq!(decode_pair_secret(&URL_SAFE.encode(raw)).unwrap(), raw);
        assert!(decode_pair_secret("not base64url!!").is_err());
        assert!(decode_pair_secret("AAAA").is_err());
    }

    /// 跨语言一致性：这个向量必须与 `server-cloudflare/scripts/generate-pair.mjs`
    /// 里用 WebCrypto HKDF 派生出的 token 完全相同（见 server-cloudflare/test/hkdf-vector.spec.ts）
    #[test]
    fn auth_token_matches_relay_generator() {
        let raw = secret([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ]);

        assert_eq!(
            URL_SAFE_NO_PAD.encode(raw),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
        );
        assert_eq!(
            derive_auth_token(&raw),
            "JSbVwoGO9GK5T58VINVkS9hNQlZMr0IYgl2_0UhEM5w"
        );
        assert_eq!(
            URL_SAFE_NO_PAD.encode(derive_root_key(&raw)),
            "am2bbPWK0R-fwuEky_8ZcFXCysV2gSD_UV6ONiWMsQk"
        );
    }
}

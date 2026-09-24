//! Pair Secret 的派生与端到端加密。
//!
//! 约定（必须与 `server-cloudflare/scripts/generate-pair.mjs` 完全一致）：
//!
//! ```text
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-auth-v1")--> PAIR_AUTH_TOKEN
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-e2ee-v1")--> E2EE_ROOT_KEY
//! PAIR_SECRET --HKDF-SHA256(info="bongocat-pair-room-v1")--> ROOM_ID
//! ```
//!
//! 每个应用帧：`header(14B) || nonce(24B) || XChaCha20-Poly1305(ciphertext + tag)`，
//! 其中 header 作为 AEAD 的 associated data 参与认证。
//!
//! `ROOM_ID`（多会话，§7）只用于**分组**：中继按它把连接划进不同的双人会话，
//! 它既不是密钥材料，也不能反推出 secret（HKDF 是单向的）。中继永远拿不到
//! secret 与 E2EE 根密钥——它只看到 `ROOM_ID` 与 `PAIR_AUTH_TOKEN`。

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand::Rng as _;
use sha2::{Digest as _, Sha256};

use super::protocol::{FRAME_HEADER_SIZE, FrameHeader, NONCE_SIZE};

pub const AUTH_INFO: &[u8] = b"bongocat-pair-auth-v1";
pub const E2EE_INFO: &[u8] = b"bongocat-pair-e2ee-v1";
pub const TRANSFER_INFO: &[u8] = b"bongocat-pair-transfer-v1";
pub const ROOM_INFO: &[u8] = b"bongocat-pair-room-v1";
pub const PAIR_SECRET_BYTES: usize = 32;
/// `ROOM_ID` 的文本形态：32 字节的 base64url 无填充，固定 43 个字符。
pub const ROOM_ID_LENGTH: usize = 43;

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

/// 派生多会话分组用的 `ROOM_ID`（base64url 无填充，43 字符）。
///
/// 两个人填同一个 Pair Secret 就派生出同一个 `ROOM_ID`，中继据此把他们放进同一个
/// 双人会话；填不同的 secret 就是两个互不可见的会话。派生方式和 token / 根密钥
/// 一样是 HKDF（不直接用 `SHA256(secret)`），这样三路输出彼此独立。
pub fn derive_room_id(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(hkdf_sha256(secret, ROOM_INFO))
}

/// 每个 transfer 一把临时密钥（R17）：
/// `HKDF-SHA256(ikm = E2EE_ROOT_KEY, salt = 空, info = "bongocat-pair-transfer-v1" || transferId(8 字节大端))`。
///
/// 只有文件传输用它，传完即弃；聊天等其它帧仍然用根密钥。这样即使某次传输的
/// nonce 被复用，也影响不到聊天内容。
pub fn derive_transfer_key(root_key: &[u8; 32], transfer_id: u64) -> [u8; 32] {
    let mut info = Vec::with_capacity(TRANSFER_INFO.len() + 8);

    info.extend_from_slice(TRANSFER_INFO);
    info.extend_from_slice(&transfer_id.to_be_bytes());

    hkdf_sha256(root_key, &info)
}

/// 解析用户粘贴的联机密钥（base64url，32 字节）
///
/// §21：用户可见的错误文案一律叫「联机密钥」；`PAIR_SECRET` 只留作内部标识。
pub fn decode_pair_secret(text: &str) -> Result<[u8; PAIR_SECRET_BYTES], String> {
    let trimmed = text.trim();

    if trimmed.is_empty() {
        return Err("联机密钥不能为空".into());
    }

    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(trimmed))
        .map_err(|_| "联机密钥不是合法的 base64url 文本".to_string())?;

    let length = bytes.len();

    bytes
        .try_into()
        .map_err(|_| format!("联机密钥应为 {PAIR_SECRET_BYTES} 字节，实际 {length} 字节"))
}

/// R17 的核对指纹：`sha256(原始 secret 字节)` 的 hex 前 16 位，每两位之间加一个空格。
///
/// 用途只是「双方肉眼核对填的是同一个 secret」——它不是密钥材料，也不是从 secret
/// 反推 secret 的途径，所以可以显示在设置页；真正的 token 与根密钥仍然只在 Rust
/// 内部派生、永不回显。
pub fn secret_fingerprint(secret: &[u8; PAIR_SECRET_BYTES]) -> String {
    let digest = Sha256::digest(secret);

    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
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

    /// R17：每个 transfer 一把临时密钥，跟根密钥和别的 transfer 都不相同，
    /// 但同一个 transferId 在两端派生出同一把
    #[test]
    fn transfer_keys_are_per_transfer_and_deterministic() {
        let root = derive_root_key(&secret([9u8; 32]));
        let first = derive_transfer_key(&root, 1);
        let again = derive_transfer_key(&root, 1);
        let second = derive_transfer_key(&root, 2);

        assert_eq!(first, again);
        assert_ne!(first, second);
        assert_ne!(first, root);
        // 大端：0x0102... 与逐字节拆分的结果必须一致，别让两端的信息串漂移
        assert_ne!(
            derive_transfer_key(&root, 0x0102_0304_0506_0708),
            derive_transfer_key(&root, 0x0807_0605_0403_0201)
        );
    }

    #[test]
    fn different_secret_fails_with_a_transfer_key() {
        let sender = PairCipher::new(&derive_transfer_key(
            &derive_root_key(&secret([1u8; 32])),
            7,
        ));
        let receiver = PairCipher::new(&derive_transfer_key(
            &derive_root_key(&secret([2u8; 32])),
            7,
        ));

        let frame = sender
            .seal(
                &FrameHeader {
                    kind: FrameKind::TransferChunk,
                    flags: 0,
                    transfer_id: 7,
                    seq: 0,
                },
                b"chunk",
            )
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

    /// 多会话（§18）：Room ID 是**固定向量**，以后不允许无意改变——两个人正是靠它
    /// 落进同一个双人会话，改了就等于把所有已配对的用户拆开。
    #[test]
    fn room_id_matches_the_fixed_vector() {
        let raw = secret([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ]);

        assert_eq!(
            derive_room_id(&raw),
            "r4iuM8zciDge4c6arhFls-s26ixDiKORe-uxFj6U97M"
        );
        assert_eq!(derive_room_id(&raw).len(), ROOM_ID_LENGTH);
    }

    #[test]
    fn the_room_id_follows_the_secret() {
        let first = secret([1u8; PAIR_SECRET_BYTES]);
        let again = secret([1u8; PAIR_SECRET_BYTES]);
        let other = secret([2u8; PAIR_SECRET_BYTES]);

        // 同一个 secret 派生同一个 Room（双方填同一个值才会进同一个会话）
        assert_eq!(derive_room_id(&first), derive_room_id(&again));
        // 不同 secret 落进不同 Room
        assert_ne!(derive_room_id(&first), derive_room_id(&other));
        // 三路输出彼此独立：Room ID 不能等于 token / 根密钥的文本形态
        assert_ne!(derive_room_id(&first), derive_auth_token(&first));
        assert_ne!(
            derive_room_id(&first),
            URL_SAFE_NO_PAD.encode(derive_root_key(&first))
        );
    }

    /// 指纹会显示给用户手工核对，格式必须固定：8 组两位小写 hex，空格分隔
    #[test]
    fn secret_fingerprint_is_stable() {
        let raw = secret([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ]);

        assert_eq!(secret_fingerprint(&raw), "63 0d cd 29 66 c4 33 66");
        assert_ne!(secret_fingerprint(&raw), secret_fingerprint(&secret([0xff; 32])));
    }
}

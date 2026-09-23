import { describe, expect, it } from 'vitest'

/**
 * 跨语言密钥派生向量。
 *
 * 生成脚本用 WebCrypto 派生鉴权 token，客户端用 Rust 的 `hkdf` crate 派生
 * （`src-tauri/src/core/pair/crypto.rs` 里有同样一组断言）。两边任何一侧改了
 * salt / info / 编码方式，都会让这里和那边同时失败。
 */
const SECRET = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8'

const AUTH_INFO = 'bongocat-pair-auth-v1'
const E2EE_INFO = 'bongocat-pair-e2ee-v1'
/** 指纹：sha256(原始 secret) 的 hex 前 16 位，两两空格分组（双方核对用） */
const FINGERPRINT = '63 0d cd 29 66 c4 33 66'

async function derive(secret: Uint8Array, info: string) {
  const key = await crypto.subtle.importKey('raw', secret, 'HKDF', false, ['deriveBits'])

  const bits = await crypto.subtle.deriveBits(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new Uint8Array(0),
      info: new TextEncoder().encode(info),
    },
    key,
    256,
  )

  return new Uint8Array(bits)
}

function base64url(bytes: Uint8Array) {
  let binary = ''

  for (const byte of bytes) binary += String.fromCharCode(byte)

  return btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')
}

describe('pair secret derivation', () => {
  it('derives the same auth token and root key as the Rust client', async () => {
    const secret = Uint8Array.from(atob(SECRET.replaceAll('-', '+').replaceAll('_', '/')), char => char.charCodeAt(0))

    expect(base64url(await derive(secret, AUTH_INFO))).toBe('JSbVwoGO9GK5T58VINVkS9hNQlZMr0IYgl2_0UhEM5w')
    expect(base64url(await derive(secret, E2EE_INFO))).toBe('am2bbPWK0R-fwuEky_8ZcFXCysV2gSD_UV6ONiWMsQk')
  })

  it('derives the same fingerprint as the generator script', async () => {
    const secret = Uint8Array.from(atob(SECRET.replaceAll('-', '+').replaceAll('_', '/')), char => char.charCodeAt(0))
    const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', secret))
    const hex = Array.from(digest, byte => byte.toString(16).padStart(2, '0')).join('').slice(0, 16)

    expect(hex.match(/.{2}/g)!.join(' ')).toBe(FINGERPRINT)
  })
})

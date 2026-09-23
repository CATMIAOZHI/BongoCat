#!/usr/bin/env node
import { Buffer } from 'node:buffer'
/**
 * Node 侧自检：确认生成脚本（也就是生产用的那条派生路径）算出的值与另外两边完全一致。
 *
 * 三处必须同时通过：
 *   1. 这里（Node / WebCrypto，`generate-pair.mjs` 真正会跑的路径）
 *   2. `test/hkdf-vector.spec.ts`（workerd / WebCrypto）
 *   3. `src-tauri/src/core/pair/crypto.rs` 的 `auth_token_matches_relay_generator`（Rust）
 *
 *   pnpm pair:selftest
 */
import process from 'node:process'

import { deriveAuthToken, deriveRootKey, encodeSecret, fingerprint } from './generate-pair.mjs'

const SECRET_TEXT = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8'
const SECRET = Buffer.from(SECRET_TEXT, 'base64url')

const expectedAuthToken = 'JSbVwoGO9GK5T58VINVkS9hNQlZMr0IYgl2_0UhEM5w'
const expectedRootKey = 'am2bbPWK0R-fwuEky_8ZcFXCysV2gSD_UV6ONiWMsQk'
const expectedFingerprint = '63 0d cd 29 66 c4 33 66'

const actualAuthToken = await deriveAuthToken(SECRET)
const actualRootKey = Buffer.from(await deriveRootKey(SECRET)).toString('base64url')
const actualFingerprint = fingerprint(SECRET)

const problems = []

if (encodeSecret(SECRET) !== SECRET_TEXT) {
  problems.push('encodeSecret 与测试向量不一致（base64url 无填充）')
}

if (actualAuthToken !== expectedAuthToken) {
  problems.push(`AUTH_TOKEN 不一致：${actualAuthToken} != ${expectedAuthToken}`)
}

if (actualAuthToken.length !== 43) {
  problems.push(`AUTH_TOKEN 应当是 43 字符的 base64url 文本，实际 ${actualAuthToken.length} 字符`)
}

if (actualRootKey !== expectedRootKey) {
  problems.push(`E2EE_ROOT_KEY 不一致：${actualRootKey} != ${expectedRootKey}`)
}

if (actualFingerprint !== expectedFingerprint) {
  problems.push(`指纹不一致：${actualFingerprint} != ${expectedFingerprint}`)
}

if (problems.length > 0) {
  console.error('pair:selftest 失败：')

  for (const problem of problems) console.error(`- ${problem}`)

  process.exit(1)
}

console.log('pair:selftest OK：AUTH_TOKEN / E2EE_ROOT_KEY / 指纹与 Rust 客户端、Workers 测试一致')

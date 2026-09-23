#!/usr/bin/env node
import { Buffer } from 'node:buffer'
import { spawn } from 'node:child_process'
/**
 * 为「一对用户」生成共享密钥，并把派生出的鉴权 token 通过 stdin 交给
 * `wrangler secret put`（不进 shell history、不落盘、不打印）。
 *
 *   node scripts/generate-pair.mjs              只生成并显示 Pair Secret
 *   node scripts/generate-pair.mjs --write      额外把 Pair Secret 写入 pair-secret.txt
 *   node scripts/generate-pair.mjs --deploy     生成后用管道写入 Cloudflare secret
 *
 * 分工：
 *   PAIR_SECRET      —— 由用户自己保存并交给对方（双方填同一个值）
 *   PAIR_AUTH_TOKEN  —— 从 PAIR_SECRET 派生，只存在于 Cloudflare 端与运行内存里
 *
 * 派生函数是导出的：`scripts/pair-selftest.mjs` 用同一组跨语言向量断言它们，
 * 免得这里的 salt / info / 编码方式悄悄漂移（那样双方都会 401）。
 */
import { createHash, randomBytes, webcrypto } from 'node:crypto'
import { writeFile } from 'node:fs/promises'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

export const AUTH_INFO = 'bongocat-pair-auth-v1'
export const E2EE_INFO = 'bongocat-pair-e2ee-v1'
export const SECRET_FILE = 'pair-secret.txt'

const base64url = bytes => Buffer.from(bytes).toString('base64url')

export async function hkdf(secret, info, length = 32) {
  const key = await webcrypto.subtle.importKey('raw', secret, 'HKDF', false, ['deriveBits'])

  const bits = await webcrypto.subtle.deriveBits(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new Uint8Array(0),
      info: new TextEncoder().encode(info),
    },
    key,
    length * 8,
  )

  return new Uint8Array(bits)
}

/** 派生鉴权 token（base64url 无填充，43 字符），必须与 Rust 客户端一致 */
export async function deriveAuthToken(secret) {
  return base64url(await hkdf(secret, AUTH_INFO))
}

/** 派生 E2EE 根密钥（32 原始字节），客户端再按 transfer 派生会话密钥 */
export async function deriveRootKey(secret) {
  return new Uint8Array(await hkdf(secret, E2EE_INFO))
}

/** Pair Secret 的文本形态 */
export function encodeSecret(secret) {
  return base64url(secret)
}

/** 双方核对是否填了同一个 secret（sha256 前 16 位，两两空格分组） */
export function fingerprint(secret) {
  return createHash('sha256').update(secret).digest('hex').slice(0, 16).match(/.{2}/g)?.join(' ')
}

function putSecret(token) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      ['node_modules/wrangler/bin/wrangler.js', 'secret', 'put', 'PAIR_AUTH_TOKEN'],
      { stdio: ['pipe', 'inherit', 'inherit'] },
    )

    child.on('error', reject)
    child.on('close', code => code === 0 ? resolve() : reject(new Error(`wrangler exited with ${code}`)))

    child.stdin.end(`${token}\n`)
  })
}

async function main() {
  const secret = randomBytes(32)
  const authToken = await deriveAuthToken(secret)
  const encodedSecret = encodeSecret(secret)

  console.log('')
  console.log('Pair Secret（双方要填同一个值，请自己保存后发给对方）：')
  console.log('')
  console.log(`  ${encodedSecret}`)
  console.log('')
  console.log(`指纹（双方配置后可比对，只用于核对是否是同一个 secret）：${fingerprint(secret)}`)
  console.log('')
  console.log('提醒：上面这行会留在终端回滚区，发给对方后可以清屏（cls / clear）。')
  console.log('服务端只会保存派生出的 PAIR_AUTH_TOKEN，它不会被打印、不会被写入文件。')
  console.log('')

  if (process.argv.includes('--write')) {
    await writeFile(SECRET_FILE, `${encodedSecret}\n`, { mode: 0o600 })

    console.log(`已写入 ${SECRET_FILE}（已在 .gitignore 中忽略）。`)
    console.log(`不再需要时请删除：Remove-Item ${SECRET_FILE}  或  rm ${SECRET_FILE}`)
    console.log('')
  }

  if (process.argv.includes('--deploy')) {
    console.log('正在通过 stdin 把 PAIR_AUTH_TOKEN 写入 Cloudflare secret…')

    await putSecret(authToken)
  } else {
    console.log('下一步：')
    console.log('  node scripts/generate-pair.mjs --deploy   （把 token 用管道写入 Cloudflare）')
    console.log('  pnpm deploy                                （部署 Worker）')
    console.log('')
  }
}

// 只在被直接执行时跑 CLI；被 import（例如自检脚本）时只导出上面的函数
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main()
}

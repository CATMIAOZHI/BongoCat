import type { Env } from './env'

import {
  DEVICE_ID_PATTERN,
  HEADER_AUTHORIZATION,
  HEADER_CLIENT,
  HEADER_PROTOCOL,
  HEALTH_PATH,
  MAX_DEVICE_ID_LENGTH,
  PROTOCOL_VERSION,
  WS_PATH,
} from './protocol'

// Durable Object 类必须从 Worker 入口模块导出，绑定才能解析到它
export { PairDurableObject } from './pair'

/** 恒定时间比较，避免通过响应时间推断 token */
async function safeEqual(left: string, right: string) {
  const encoder = new TextEncoder()
  const leftBytes = encoder.encode(left)
  const rightBytes = encoder.encode(right)

  if (leftBytes.byteLength !== rightBytes.byteLength) return false

  const subtle = crypto.subtle as SubtleCrypto & {
    timingSafeEqual?: (a: ArrayBufferView, b: ArrayBufferView) => boolean
  }

  if (subtle.timingSafeEqual) return subtle.timingSafeEqual(leftBytes, rightBytes)

  let diff = 0

  for (let index = 0; index < leftBytes.byteLength; index++) {
    diff |= leftBytes[index] ^ rightBytes[index]
  }

  return diff === 0
}

/** 整串匹配：`Bearer <token>` 之外不接受任何多余内容（`split(' ')` 会放过 `Bearer x junk`） */
const BEARER_PATTERN = /^Bearer\s+(\S+)$/i

function bearerToken(request: Request) {
  const match = BEARER_PATTERN.exec((request.headers.get(HEADER_AUTHORIZATION) ?? '').trim())

  return match?.[1] ?? ''
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const { pathname } = new URL(request.url)

    if (pathname === HEALTH_PATH) {
      return Response.json({ ok: true, protocol: PROTOCOL_VERSION })
    }

    if (pathname !== WS_PATH) {
      return new Response('not found', { status: 404 })
    }

    // 不需要校验 HTTP 方法：按 RFC 6455 握手只能是 GET，运行时也会把带 Upgrade 的
    // 请求当握手处理（测试里用 POST + Upgrade: websocket 发过来，到达 Worker 时
    // request.method 仍然是 "GET"），Durable Object 那边同理。
    if (request.headers.get('Upgrade')?.toLowerCase() !== 'websocket') {
      return new Response('expected websocket upgrade', { status: 426 })
    }

    if (request.headers.get(HEADER_PROTOCOL) !== String(PROTOCOL_VERSION)) {
      return new Response('unsupported protocol', { status: 426 })
    }

    const token = bearerToken(request)

    // 先鉴权再看 deviceId：未鉴权的请求不该能探测 deviceId 的合法性
    if (!token || !await safeEqual(token, env.PAIR_AUTH_TOKEN)) {
      return new Response('authentication failed', { status: 401 })
    }

    // 归一成小写：同一个 UUID 用大写重连时不能被当成第三个人而自锁
    const deviceId = (request.headers.get(HEADER_CLIENT) ?? '').toLowerCase()

    if (
      !deviceId
      || deviceId.length > MAX_DEVICE_ID_LENGTH
      || !DEVICE_ID_PATTERN.test(deviceId)
    ) {
      return new Response('invalid client id', { status: 400 })
    }

    // Durable Object 不需要 Authorization，转发前摘掉，避免 token 进入第二个
    // 执行环境（将来谁加一行 header 日志就会泄露）。
    // 同时把 deviceId 换成归一后的值，让 DO 直接用同一个字符串比较。
    const headers = new Headers(request.headers)

    headers.delete(HEADER_AUTHORIZATION)
    headers.set(HEADER_CLIENT, deviceId)

    return env.PAIR.getByName('pair').fetch(new Request(request, { headers }))
  },
} satisfies ExportedHandler<Env>

import { DurableObject } from 'cloudflare:workers'

import type { Env } from './env'
import type { SocketAttachment } from './protocol'

import {
  CLOSE_CODE,
  FRAME_HEADER_SIZE,
  FRAME_KIND,
  HEADER_CLIENT,
  isKnownFrameKind,
  LAST_SEEN_WRITE_INTERVAL_MS,
  MAX_BINARY_FRAME_SIZE,
  MAX_BYTES_PER_SECOND,
  MAX_CHUNKS_PER_SECOND,
  MAX_FRAMES_PER_SECOND,
  PAIR_SIZE,
  PROTOCOL_VERSION,
  RATE_WINDOW_MS,
  serverFrame,
  STALE_AFTER_MS,
} from './protocol'

/**
 * 令牌桶：`frames` / `chunks` / `bytes` 是当前剩余的额度，按经过的时间连续补充
 * （容量 = 每秒上限，见 protocol.ts 的 RATE_WINDOW_MS）。
 *
 * 桶只放内存：Durable Object 休眠后丢失，但休眠要求所有连接静默至少一个
 * 休眠超时，那时限流窗口本来也已经过去了。跨休眠必须保留的只有 deviceId 与
 * 最后活动时间（见 docs/pair-plan.md 的 R9）。
 */
interface RateBucket {
  frames: number
  chunks: number
  bytes: number
  updatedAt: number
}

const RATE_CAPACITY = {
  frames: MAX_FRAMES_PER_SECOND,
  chunks: MAX_CHUNKS_PER_SECOND,
  bytes: MAX_BYTES_PER_SECOND,
} as const

/**
 * 固定双人中继。只做三件事：维持两条 WebSocket、A↔B 转发、把 peer 上下线
 * 以明文控制帧告知对方。不保存用户内容，也不解析应用负载。
 */
export class PairDurableObject extends DurableObject<Env> {
  private readonly buckets = new Map<WebSocket, RateBucket>()

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get('Upgrade')?.toLowerCase() !== 'websocket') {
      return new Response('expected websocket upgrade', { status: 426 })
    }

    const deviceId = request.headers.get(HEADER_CLIENT) ?? ''
    const now = Date.now()
    const live = this.liveSockets()
    const mine = live.filter(socket => this.attachmentOf(socket)?.deviceId === deviceId)
    const peers = live.filter(socket => !mine.includes(socket))

    // 先算出「顶替之后还剩下几个对端」，再决定是否动手关连接：先关再拒绝会把
    // 发起方自己原来的连接也关掉，让本来能恢复的情况变成完全连不上。
    const stale = peers.length >= PAIR_SIZE
      ? peers.find((socket) => {
          const lastSeen = this.attachmentOf(socket)?.lastSeen ?? now

          return now - lastSeen > STALE_AFTER_MS
        })
      : undefined
    const remaining = stale ? peers.filter(socket => socket !== stale) : peers

    if (remaining.length >= PAIR_SIZE) {
      // 持有同一个 PAIR_AUTH_TOKEN 的第三方无法进入：这是体验约束，
      // 不是安全边界（见 docs/pair-plan.md 的 R9）。
      const rejected = new WebSocketPair()

      rejected[1].accept()
      rejected[1].close(CLOSE_CODE.PAIR_FULL, 'pair is full')

      return new Response(null, { status: 101, webSocket: rejected[0] })
    }

    // 同一 deviceId 重连（例如切换网络）：关掉旧连接再接受新连接，
    // 这样网络切换后不会把旧连接算成第三个人。
    for (const socket of mine) {
      this.closeQuietly(socket, CLOSE_CODE.REPLACED, 'replaced by a newer connection')
    }

    if (stale) {
      this.closeQuietly(stale, CLOSE_CODE.STALE, 'stale connection replaced')
    }

    return this.accept(deviceId, remaining)
  }

  private accept(deviceId: string, peers: WebSocket[]) {
    const pair = new WebSocketPair()
    const server = pair[1]

    this.ctx.acceptWebSocket(server)

    const attachment: SocketAttachment = {
      deviceId,
      lastSeen: Date.now(),
    }

    server.serializeAttachment(attachment)
    server.send(serverFrame({
      type: 'server.welcome',
      protocol: PROTOCOL_VERSION,
      peerOnline: peers.length > 0,
    }))

    for (const peer of peers) {
      this.sendQuietly(peer, serverFrame({ type: 'server.peer', online: true, deviceId }))
    }

    return new Response(null, { status: 101, webSocket: pair[0] })
  }

  async webSocketMessage(socket: WebSocket, message: string | ArrayBuffer) {
    const attachment = this.attachmentOf(socket)

    if (!attachment) {
      this.closeQuietly(socket, CLOSE_CODE.INTERNAL_ERROR, 'missing attachment')

      return
    }

    if (typeof message === 'string') {
      // text 帧只用于服务端 → 客户端的控制帧（server.welcome / server.peer）。
      // 客户端发 text 属于协议偏离：若原样转发，已配对的一方能伪造 server.*
      // 控制帧（例如谎报对方上下线），而且这条路径不经过 AEAD。
      this.closeQuietly(socket, CLOSE_CODE.PROTOCOL_ERROR, 'client text frames are not accepted')

      return
    }

    const bytes = new Uint8Array(message)

    if (bytes.byteLength > MAX_BINARY_FRAME_SIZE) {
      this.closeQuietly(socket, CLOSE_CODE.TOO_LARGE, 'frame too large')

      return
    }

    if (bytes.byteLength < FRAME_HEADER_SIZE) {
      this.closeQuietly(socket, CLOSE_CODE.PROTOCOL_ERROR, 'malformed frame')

      return
    }

    const kind = bytes[0]

    if (!isKnownFrameKind(kind)) {
      this.closeQuietly(socket, CLOSE_CODE.PROTOCOL_ERROR, 'unknown frame kind')

      return
    }

    const isChunk = kind === FRAME_KIND.TRANSFER_CHUNK

    if (!this.allow(socket, { frames: 1, chunks: isChunk ? 1 : 0, bytes: bytes.byteLength })) {
      return
    }

    this.touch(socket, attachment)
    this.forward(socket, message)
  }

  async webSocketClose(socket: WebSocket) {
    this.buckets.delete(socket)
    this.announceOffline(socket)
  }

  async webSocketError(socket: WebSocket) {
    this.buckets.delete(socket)
    this.announceOffline(socket)
  }

  private announceOffline(socket: WebSocket) {
    const deviceId = this.attachmentOf(socket)?.deviceId

    if (!deviceId) return

    // 同一 deviceId 的新连接已经在同一时刻挂上来时（顶替重连），这条离线通知是
    // 伪广播：对端会看到「上线 → 下线」，最终以为对方离线。正在顶替的旧连接
    // 关闭事件总是晚于新连接被接受，所以这里必须按 deviceId 过滤。
    const replaced = this.liveSockets().some(
      peer => peer !== socket && this.attachmentOf(peer)?.deviceId === deviceId,
    )

    if (replaced) return

    for (const peer of this.liveSockets()) {
      if (peer === socket) continue

      this.sendQuietly(peer, serverFrame({ type: 'server.peer', online: false, deviceId }))
    }
  }

  /** 只转发给对端，不回发给发送者 */
  private forward(socket: WebSocket, message: string | ArrayBuffer) {
    for (const peer of this.liveSockets()) {
      if (peer === socket) continue

      this.sendQuietly(peer, message)
    }
  }

  private allow(socket: WebSocket, delta: { frames?: number, chunks?: number, bytes?: number }) {
    const now = Date.now()
    let bucket = this.buckets.get(socket)

    if (!bucket) {
      bucket = {
        frames: RATE_CAPACITY.frames,
        chunks: RATE_CAPACITY.chunks,
        bytes: RATE_CAPACITY.bytes,
        updatedAt: now,
      }
      this.buckets.set(socket, bucket)
    } else {
      const elapsed = Math.max(0, now - bucket.updatedAt)

      bucket.updatedAt = now
      bucket.frames = Math.min(RATE_CAPACITY.frames, bucket.frames + elapsed * RATE_CAPACITY.frames / RATE_WINDOW_MS)
      bucket.chunks = Math.min(RATE_CAPACITY.chunks, bucket.chunks + elapsed * RATE_CAPACITY.chunks / RATE_WINDOW_MS)
      bucket.bytes = Math.min(RATE_CAPACITY.bytes, bucket.bytes + elapsed * RATE_CAPACITY.bytes / RATE_WINDOW_MS)
    }

    bucket.frames -= delta.frames ?? 0
    bucket.chunks -= delta.chunks ?? 0
    bucket.bytes -= delta.bytes ?? 0

    if (bucket.frames < 0 || bucket.chunks < 0 || bucket.bytes < 0) {
      this.closeQuietly(socket, CLOSE_CODE.PROTOCOL_ERROR, 'rate limit exceeded')

      return false
    }

    return true
  }

  /** 最后活动时间最多每 10 秒写一次 */
  private touch(socket: WebSocket, attachment: SocketAttachment) {
    const now = Date.now()

    if (now - attachment.lastSeen < LAST_SEEN_WRITE_INTERVAL_MS) return

    attachment.lastSeen = now
    socket.serializeAttachment(attachment)
  }

  private liveSockets() {
    return this.ctx.getWebSockets().filter(socket => socket.readyState === WebSocket.OPEN)
  }

  private attachmentOf(socket: WebSocket) {
    return socket.deserializeAttachment() as SocketAttachment | null
  }

  private closeQuietly(socket: WebSocket, code: number, reason: string) {
    try {
      socket.close(code, reason)
    } catch {
      // 连接已经不可用，忽略
    }
  }

  private sendQuietly(socket: WebSocket, message: string | ArrayBuffer) {
    try {
      socket.send(message)
    } catch {
      // 对端断开，忽略
    }
  }
}

/**
 * BongoCat pair relay protocol (version 1).
 *
 * The relay is deliberately dumb: it authenticates the pair, keeps exactly two
 * WebSockets alive, and forwards frames between them. It never stores user
 * content and never decrypts application payloads.
 */

export const PROTOCOL_VERSION = 1

/** `GET /health` */
export const HEALTH_PATH = '/health'

/** `GET /ws` — WebSocket upgrade endpoint */
export const WS_PATH = '/ws'

export const HEADER_AUTHORIZATION = 'authorization'
export const HEADER_CLIENT = 'x-bongo-client'
export const HEADER_PROTOCOL = 'x-bongo-protocol'

/** 每个应用帧固定 14 字节明文帧头：kind(1) | flags(1) | transferId(8) | seq(4) */
export const FRAME_HEADER_SIZE = 14

/**
 * 明文帧头只用于让 relay 分桶限流；帧头同时必须作为 AEAD 的 associated data
 * 参与认证，否则中转方可以改 kind 绕开限流（见 docs/pair-plan.md 的 R8）。
 */
export const FRAME_KIND = {
  PET_STATE: 1,
  PRESENCE: 2,
  STATS: 3,
  CHAT: 4,
  TRANSFER_CONTROL: 5,
  TRANSFER_CHUNK: 6,
  ACK: 7,
  PING: 8,
} as const

export const KNOWN_FRAME_KINDS: ReadonlySet<number> = new Set(Object.values(FRAME_KIND))

/**
 * 单帧上限（整帧，含帧头与 nonce/tag）。
 *
 * 只对客户端发来的 binary 帧生效：text 帧在协议里只用于服务端 → 客户端的控制帧，
 * 客户端发 text 一律按协议错误关闭（见 pair.ts 的 webSocketMessage）。
 */
export const MAX_BINARY_FRAME_SIZE = 1024 * 1024

/** 一对用户永远只有两个连接 */
export const PAIR_SIZE = 2

/**
 * 限流参数：容量就是下面三个「每秒上限」，按时间连续补充（令牌桶，见 pair.ts 的 allow）。
 *
 * 用令牌桶而不是固定窗口，是因为固定窗口在边界会允许双倍突发，而 R4 要求
 * 「状态变化立即发送」，一次抖动就可能被误判成超限并关掉连接。
 */
export const RATE_WINDOW_MS = 1000
export const MAX_FRAMES_PER_SECOND = 30
export const MAX_CHUNKS_PER_SECOND = 20
/**
 * 12 MiB：20 个 512 KiB chunk（含帧头与 nonce/tag 每个约 524 KiB）合计约 10 MiB
 * （10.49 MB），字节上限必须容得下这个突发，否则文件传一半就会被关连接。
 */
export const MAX_BYTES_PER_SECOND = 12 * 1024 * 1024

/** 超过这个时间没有任何消息的连接可以被新连接顶替（2 倍心跳） */
export const STALE_AFTER_MS = 120 * 1000

/** 最后活动时间最多每 10 秒写一次 attachment，避免高频写 */
export const LAST_SEEN_WRITE_INTERVAL_MS = 10 * 1000

export const MAX_DEVICE_ID_LENGTH = 64
export const DEVICE_ID_PATTERN = /^[A-Z0-9-]+$/i

export const CLOSE_CODE = {
  /** 同一 deviceId 重连：旧连接被顶替 */
  REPLACED: 4002,
  /** 第三个不同的客户端 */
  PAIR_FULL: 4003,
  /** 顶替长时间无活动的连接 */
  STALE: 4004,
  /** 协议/帧格式错误 */
  PROTOCOL_ERROR: 1008,
  /** 帧过大 */
  TOO_LARGE: 1009,
  /** 服务端内部错误 */
  INTERNAL_ERROR: 1011,
} as const

/** 服务端控制帧（明文 JSON，不含任何用户内容） */
export interface ServerWelcomeFrame {
  type: 'server.welcome'
  protocol: number
  peerOnline: boolean
}

export interface ServerPeerFrame {
  type: 'server.peer'
  online: boolean
  deviceId: string
}

export interface ServerErrorFrame {
  type: 'server.error'
  code: string
  message: string
}

/** `server.error` 目前是预留类型：中继只用关闭码表达错误，不会发送它 */
export type ServerFrame = ServerWelcomeFrame | ServerPeerFrame | ServerErrorFrame

/**
 * 每个连接需要跨休眠保留的最小状态。
 *
 * 限流计数故意不放在这里：它们只存在于内存，休眠后重置；而休眠要求所有连接
 * 静默至少一个休眠超时，那时限流窗口早已过去。
 */
export interface SocketAttachment {
  deviceId: string
  lastSeen: number
}

export function isKnownFrameKind(kind: number) {
  return KNOWN_FRAME_KINDS.has(kind)
}

export function serverFrame(frame: ServerFrame) {
  return JSON.stringify(frame)
}

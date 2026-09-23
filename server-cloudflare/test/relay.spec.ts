import { env, runInDurableObject, SELF } from 'cloudflare:test'
import { afterEach, describe, expect, it } from 'vitest'

import {
  CLOSE_CODE,
  FRAME_HEADER_SIZE,
  FRAME_KIND,
  MAX_BINARY_FRAME_SIZE,
  MAX_CHUNKS_PER_SECOND,
  MAX_FRAMES_PER_SECOND,
  PROTOCOL_VERSION,
} from '../src/protocol'

const TOKEN = 'test-token'
const ENDPOINT = 'https://relay.test/ws'
/** 应用帧还有 nonce(24B) 与 AEAD tag(16B)，这里按真实长度构造 */
const CHUNK_FRAME_SIZE = FRAME_HEADER_SIZE + 24 + 512 * 1024 + 16

interface Connection {
  deviceId: string
  socket: WebSocket
  texts: string[]
  binary: Uint8Array[]
  closed: Promise<{ code: number, reason: string }>
}

const opened: Connection[] = []

function upgrade(deviceId: string, init: { token?: string, protocol?: string } = {}) {
  return SELF.fetch(ENDPOINT, {
    headers: {
      'Upgrade': 'websocket',
      'Authorization': `Bearer ${init.token ?? TOKEN}`,
      'X-Bongo-Client': deviceId,
      'X-Bongo-Protocol': init.protocol ?? String(PROTOCOL_VERSION),
    },
  })
}

async function connect(deviceId: string, init?: { token?: string, protocol?: string }) {
  const response = await upgrade(deviceId, init)
  const socket = response.webSocket

  if (response.status !== 101 || !socket) {
    throw new Error(`expected 101 websocket upgrade, got ${response.status}`)
  }

  const connection: Connection = {
    deviceId,
    socket,
    texts: [],
    binary: [],
    closed: new Promise((resolve) => {
      socket.addEventListener('close', event => resolve({ code: event.code, reason: event.reason }))
    }),
  }

  // WHATWG WebSocket 默认 binaryType 是 blob，这里统一成 ArrayBuffer 便于断言
  socket.binaryType = 'arraybuffer'

  socket.addEventListener('message', (event) => {
    if (typeof event.data === 'string') {
      connection.texts.push(event.data)
    } else if (event.data instanceof ArrayBuffer) {
      connection.binary.push(new Uint8Array(event.data))
    }
  })

  socket.accept()
  opened.push(connection)

  return connection
}

function frame(kind: number, payloadSize = 32) {
  const bytes = new Uint8Array(14 + 24 + payloadSize)
  bytes[0] = kind

  return bytes
}

function chunkFrame() {
  const bytes = new Uint8Array(CHUNK_FRAME_SIZE)
  bytes[0] = FRAME_KIND.TRANSFER_CHUNK

  return bytes
}

/** 只占字节桶的满尺寸帧（1 MiB 整，含帧头与 nonce/tag） */
function fullSizeFrame() {
  const bytes = new Uint8Array(MAX_BINARY_FRAME_SIZE)
  bytes[0] = FRAME_KIND.PET_STATE

  return bytes
}

async function waitFor(predicate: () => boolean, timeout = 3000) {
  const deadline = Date.now() + timeout

  while (Date.now() < deadline) {
    if (predicate()) return

    await new Promise(resolve => setTimeout(resolve, 10))
  }

  throw new Error('timed out waiting for condition')
}

/** Durable Object 里的 OPEN 连接数，用来判断关闭事件是否已经处理完 */
function openSocketCount() {
  return runInDurableObject(env.PAIR.getByName('pair'), (_instance, state) =>
    state.getWebSockets().filter(socket => socket.readyState === WebSocket.OPEN).length)
}

async function waitForOpenSockets(count: number, timeout = 3000) {
  const deadline = Date.now() + timeout
  let last = -1

  while (Date.now() < deadline) {
    last = await openSocketCount()

    if (last === count) return

    await new Promise(resolve => setTimeout(resolve, 10))
  }

  throw new Error(`timed out waiting for ${count} open socket(s), last saw ${last}`)
}

function peerFrames(connection: Connection, online?: boolean) {
  return connection.texts
    .map(text => JSON.parse(text))
    .filter(frame => frame.type === 'server.peer' && (online === undefined || frame.online === online))
}

afterEach(async () => {
  for (const connection of opened.splice(0)) {
    try {
      connection.socket.close(1000, 'test cleanup')
    } catch {
      // 已经关闭
    }
  }

  // 同一个 Durable Object 实例跨用例复用，必须等到真的没有残留连接；
  // 固定的 50ms 睡眠在慢机器上会让残留连接被下一个用例算成对端
  const deadline = Date.now() + 2000

  while (Date.now() < deadline) {
    if (await openSocketCount() === 0) return

    await new Promise(resolve => setTimeout(resolve, 10))
  }

  // 残留连接会污染下一个用例（同一个 DO 实例跨用例复用），必须显式失败而不是静默放过
  throw new Error('reusable Durable Object still had open sockets after cleanup')
})

describe('worker authentication', () => {
  it('reports health without leaking pair state', async () => {
    const response = await SELF.fetch('https://relay.test/health')

    expect(response.status).toBe(200)
    expect(await response.json()).toEqual({ ok: true, protocol: PROTOCOL_VERSION })
  })

  it('rejects a plain HTTP request to /ws', async () => {
    const response = await SELF.fetch(ENDPOINT)

    expect(response.status).toBe(426)
  })

  it('rejects a missing or wrong token', async () => {
    const missing = await SELF.fetch(ENDPOINT, {
      headers: {
        'Upgrade': 'websocket',
        'X-Bongo-Client': 'device-a',
        'X-Bongo-Protocol': '1',
      },
    })
    const wrong = await upgrade('device-a', { token: 'nope' })

    expect(missing.status).toBe(401)
    expect(wrong.status).toBe(401)
  })

  it('rejects a bearer header with extra segments', async () => {
    const response = await SELF.fetch(ENDPOINT, {
      headers: {
        'Upgrade': 'websocket',
        'Authorization': `Bearer ${TOKEN} junk`,
        'X-Bongo-Client': 'device-a',
        'X-Bongo-Protocol': '1',
      },
    })

    expect(response.status).toBe(401)
  })

  it('rejects an unsupported protocol version', async () => {
    const response = await upgrade('device-a', { protocol: '2' })

    expect(response.status).toBe(426)
  })

  it('rejects an invalid device id', async () => {
    const response = await upgrade('bad device id!')

    expect(response.status).toBe(400)
  })

  it('returns 404 for unknown paths', async () => {
    const response = await SELF.fetch('https://relay.test/nope')

    expect(response.status).toBe(404)
  })
})

describe('two-person relay', () => {
  it('welcomes the first client as peer-offline', async () => {
    const a = await connect('device-a')

    await waitFor(() => a.texts.length > 0)

    expect(JSON.parse(a.texts[0])).toEqual({
      type: 'server.welcome',
      protocol: PROTOCOL_VERSION,
      peerOnline: false,
    })
  })

  it('tells both sides when a peer joins', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => a.texts.length > 0)
    await waitFor(() => b.texts.length > 0)
    await waitFor(() => peerFrames(a, true).length > 0)

    expect(JSON.parse(b.texts[0]).peerOnline).toBe(true)
    expect(peerFrames(a, true)[0].deviceId).toBe('device-b')
  })

  it('forwards binary frames only to the peer', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => b.texts.length > 0)

    a.socket.send(frame(FRAME_KIND.PING))
    await waitFor(() => b.binary.length > 0)

    expect(b.binary[0][0]).toBe(FRAME_KIND.PING)
    expect(a.binary).toHaveLength(0)
  })

  it('rejects a third distinct client with 4003', async () => {
    await connect('device-a')
    await connect('device-b')
    const c = await connect('device-c')

    const closed = await c.closed

    expect(closed.code).toBe(CLOSE_CODE.PAIR_FULL)
    expect(closed.reason).toContain('pair is full')
  })

  it('replaces an existing connection with the same device id', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')
    const a2 = await connect('device-a')

    expect((await a.closed).code).toBe(CLOSE_CODE.REPLACED)
    await waitFor(() => a2.texts.length > 0)
    expect(JSON.parse(a2.texts[0]).peerOnline).toBe(true)

    // 旧的 a 被顶替后，b 仍然是唯一对端；第三个不同 id 依然被拒绝
    const c = await connect('device-c')

    expect((await c.closed).code).toBe(CLOSE_CODE.PAIR_FULL)
    expect(b.socket.readyState).toBe(WebSocket.OPEN)
  })

  it('does not broadcast a false offline frame when a device reconnects', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')
    const a2 = await connect('device-a')

    expect((await a.closed).code).toBe(CLOSE_CODE.REPLACED)
    await waitFor(() => a2.texts.length > 0)

    // 等 DO 里只剩新 A 与 B 两条 OPEN 连接：说明旧连接的关闭事件已经处理完，
    // 此时断言「没有伪离线」才是有意义的（否则只是还没到）
    await waitForOpenSockets(2)

    expect(peerFrames(b, false)).toHaveLength(0)
    expect(peerFrames(a2, false)).toHaveLength(0)
    expect(peerFrames(b, true).at(-1)?.deviceId).toBe('device-a')
  })

  it('treats a device id differing only in case as the same device', async () => {
    const a = await connect('device-a')
    const upper = await connect('DEVICE-A')

    // 同一台设备的大写重连应当顶替旧连接（4002），而不是被当成第三个人（4003）
    expect((await a.closed).code).toBe(CLOSE_CODE.REPLACED)

    await waitForOpenSockets(1)
    expect(upper.socket.readyState).toBe(WebSocket.OPEN)
  })

  it('lets a stale connection be replaced', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await runInDurableObject(env.PAIR.getByName('pair'), (_instance, state) => {
      for (const socket of state.getWebSockets()) {
        const attachment = socket.deserializeAttachment() as { deviceId: string, lastSeen: number } | null

        if (!attachment) continue

        attachment.lastSeen = Date.now() - 10 * 60 * 1000
        socket.serializeAttachment(attachment)
      }
    })

    const c = await connect('device-c')

    await waitFor(() => c.texts.length > 0)
    expect(JSON.parse(c.texts[0]).peerOnline).toBe(true)

    const closed = await Promise.race([a.closed, b.closed])

    expect(closed.code).toBe(CLOSE_CODE.STALE)

    // 存活的一方应当收到被顶替那台设备的离线通知
    await waitFor(() => peerFrames(a, false).length + peerFrames(b, false).length === 1)
  })

  it('tells the peer when a client disconnects', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => peerFrames(a, true).length > 0)

    b.socket.close(1000, 'bye')
    await waitFor(() => peerFrames(a, false).length > 0)

    expect(peerFrames(a, false)[0].deviceId).toBe('device-b')
  })
})

describe('frame validation', () => {
  it('closes an oversized binary frame with 1009', async () => {
    const a = await connect('device-a')

    a.socket.send(new Uint8Array(MAX_BINARY_FRAME_SIZE + 1))

    expect((await a.closed).code).toBe(CLOSE_CODE.TOO_LARGE)
  })

  it('closes a frame shorter than the plaintext header with 1008', async () => {
    const a = await connect('device-a')

    a.socket.send(new Uint8Array(13))

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)
  })

  it('closes a frame with an unknown kind with 1008', async () => {
    const a = await connect('device-a')

    a.socket.send(frame(99))

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)
  })

  it('closes a client that exceeds the frame rate limit with 1008', async () => {
    const a = await connect('device-a')

    // 令牌桶容量就是 30 帧/秒，突发期间还会按时间补充额度，
    // 所以要明显超过「容量 + 补充」才会被判超限
    for (let index = 0; index < MAX_FRAMES_PER_SECOND * 4; index++) {
      a.socket.send(frame(FRAME_KIND.PET_STATE))
    }

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)
  })

  it('keeps a client that stays within the frame rate limit', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => b.texts.length > 0)

    for (let index = 0; index < MAX_FRAMES_PER_SECOND; index++) {
      a.socket.send(frame(FRAME_KIND.PET_STATE))
    }

    await waitFor(() => b.binary.length === MAX_FRAMES_PER_SECOND)

    expect(a.socket.readyState).toBe(WebSocket.OPEN)
  })

  it('allows a full burst of 512 KiB chunks but closes the next one', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => b.texts.length > 0)

    // 一次性发完：中间不能有 await，否则第 21 个 chunk 到达 DO 时可能已经
    // 拿到补充的额度（chunk 桶每 50ms 补 1 个），用例会变成 flaky
    for (let index = 0; index <= MAX_CHUNKS_PER_SECOND; index++) {
      a.socket.send(chunkFrame())
    }

    // 对端只应该收到前 20 个，第 21 个触发 close 1008
    await waitFor(() => b.binary.length === MAX_CHUNKS_PER_SECOND, 10000)

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)
  })

  it('closes a client that exceeds the byte rate limit with 1008', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => b.texts.length > 0)

    // 只耗字节桶：1 MiB 的 pet state 帧不占 chunk 桶，
    // 13 个就超过 12 MiB/秒（14 个留出余量，避免边界抖动）
    for (let index = 0; index < 14; index++) {
      a.socket.send(fullSizeFrame())
    }

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)
  })

  it('rejects a client text frame with 1008', async () => {
    const a = await connect('device-a')
    const b = await connect('device-b')

    await waitFor(() => b.texts.length > 0)

    // 尝试伪造一条服务端控制帧
    a.socket.send(JSON.stringify({ type: 'server.peer', online: false, deviceId: 'device-b' }))

    expect((await a.closed).code).toBe(CLOSE_CODE.PROTOCOL_ERROR)

    // 而且不能被转发给对端（否则对端会把它当成服务端控制帧）
    expect(b.texts.filter(text => text.includes('server.peer'))).toHaveLength(0)
    expect(b.binary).toHaveLength(0)
  })
})

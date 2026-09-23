import { describe, expect, it } from 'vitest'

import type { ChatMessage } from './usePair'

import { createVoiceSession } from './usePairVoice'

function message(): ChatMessage {
  return {
    seq: 1,
    id: 'id-1',
    direction: 'outgoing',
    kind: 'voice',
    createdAt: 1_700_000_000_000,
    status: 'pending',
    attachmentId: 'attachment-1',
    conversationEpoch: 1,
  }
}

function delay(ms = 0) {
  return new Promise(resolve => setTimeout(resolve, ms))
}

/** Rust 侧的命令失败时是 reject，不是 throw */
function fail(message: string): Promise<never> {
  return Promise.reject(new Error(message))
}

function recorder() {
  const calls: string[] = []
  const errors: string[] = []
  let skipped = 0

  return { calls, errors, skipped: () => skipped, onSkipped: () => skipped += 1 }
}

describe('按住说话的命令顺序', () => {
  it('松开一定排在按下之后，不会先拿到「没在录音」', async () => {
    const log = recorder()
    const session = createVoiceSession(
      {
        start: async () => {
          await delay(20)
          log.calls.push('start')
        },
        stop: async () => {
          log.calls.push('stop')

          return message()
        },
        cancel: async () => {
          log.calls.push('cancel')
        },
      },
      { onError: reason => log.errors.push(reason), onSkipped: log.onSkipped },
    )

    // 按下之后立刻松开：麦克风还没打开
    void session.press()
    void session.release()

    await session.settled()

    expect(log.calls).toEqual(['start', 'stop'])
    expect(log.skipped()).toBe(0)
  })

  it('取消也排在按下之后，不然录音线程会留在后台没人关', async () => {
    const log = recorder()
    const session = createVoiceSession(
      {
        start: async () => {
          await delay(20)
          log.calls.push('start')
        },
        stop: async () => {
          log.calls.push('stop')

          return null
        },
        cancel: async () => {
          log.calls.push('cancel')
        },
      },
      { onError: reason => log.errors.push(reason), onSkipped: log.onSkipped },
    )

    void session.press()
    void session.cancel()
    // 松开在取消之后到：这时候已经没在录了，不该再冒一句「没发出去」
    void session.release()

    await session.settled()

    expect(log.calls).toEqual(['start', 'cancel', 'stop'])
    expect(log.skipped()).toBe(0)
  })
})

describe('轻点与失败不是同一件事', () => {
  it('录到了但太短（stop 返回 null）要提示没发出去', async () => {
    const log = recorder()
    const session = createVoiceSession(
      {
        start: async () => void 0,
        stop: async () => null,
        cancel: async () => void 0,
      },
      { onError: reason => log.errors.push(reason), onSkipped: log.onSkipped },
    )

    await session.press()
    await session.release()
    await session.settled()

    expect(log.skipped()).toBe(1)
    expect(log.errors).toEqual([])
  })

  it('麦克风打不开时只报错，松开不该再补一句「没发出去」', async () => {
    const log = recorder()
    const session = createVoiceSession(
      {
        start: () => fail('找不到可用的麦克风'),
        stop: async () => null,
        cancel: async () => void 0,
      },
      { onError: reason => log.errors.push(reason), onSkipped: log.onSkipped },
    )

    await session.press()
    await session.release()
    await session.settled()

    expect(log.errors).toEqual(['找不到可用的麦克风'])
    expect(log.skipped()).toBe(0)
  })

  it('发送失败（stop 抛错）照样报给用户', async () => {
    const log = recorder()
    const session = createVoiceSession(
      {
        start: async () => void 0,
        stop: () => fail('未连接'),
        cancel: async () => void 0,
      },
      { onError: reason => log.errors.push(reason), onSkipped: log.onSkipped },
    )

    await session.press()
    await session.release()
    await session.settled()

    expect(log.errors).toEqual(['未连接'])
    expect(log.skipped()).toBe(0)
  })
})

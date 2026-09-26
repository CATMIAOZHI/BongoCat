import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { createKeyAutoRelease } from './keyAutoRelease'

const DELAY_MS = 3000

function createHarness(stillDown: (rawKeys: string[]) => boolean = () => false) {
  const released: string[] = []
  const stillDownCalls: string[][] = []
  const stillDownKeys: string[] = []

  const autoRelease = createKeyAutoRelease({
    delay: () => DELAY_MS,
    isKeyStillDown: async (rawKeys) => {
      stillDownCalls.push([...rawKeys])

      for (const key of rawKeys) {
        if (!stillDownKeys.includes(key)) stillDownKeys.push(key)
      }

      return stillDown(rawKeys)
    },
    onRelease: key => released.push(key),
  })

  return { autoRelease, released, stillDownCalls, stillDownKeys }
}

describe('按键自动释放', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  /**
   * 用户报的 bug（R46）：按住 w、中间按过 a/d（w 的自动重复就此停住）、最后只有 w 按着，
   * 高亮里却没有 w。根因就是这里把「安静」当成了「抬起」。
   */
  it('还按着就不释放：安静不等于抬起', async () => {
    const { autoRelease, released, stillDownCalls } = createHarness(() => true)

    autoRelease.press('KeyW', 'KeyW')

    await vi.advanceTimersByTimeAsync(DELAY_MS)

    expect(stillDownCalls).toEqual([['KeyW']])
    expect(released).toEqual([])

    // 续了一轮：再等一个延迟仍然在问，而不是放手
    await vi.advanceTimersByTimeAsync(DELAY_MS)

    expect(stillDownCalls).toEqual([['KeyW'], ['KeyW']])
    expect(released).toEqual([])
  })

  it('真的抬起了（系统说不在按下）就释放一次', async () => {
    const { autoRelease, released } = createHarness(() => false)

    autoRelease.press('KeyW', 'KeyW')

    await vi.advanceTimersByTimeAsync(DELAY_MS)

    expect(released).toEqual(['KeyW'])

    // 释放过就不再重复
    await vi.advanceTimersByTimeAsync(DELAY_MS * 3)

    expect(released).toEqual(['KeyW'])
  })

  it('到点前收到抬起事件就撤掉这一轮，不再问也不再释放', async () => {
    const { autoRelease, released, stillDownCalls } = createHarness(() => true)

    autoRelease.press('KeyW', 'KeyW')

    await vi.advanceTimersByTimeAsync(DELAY_MS - 1)

    autoRelease.release('KeyW')

    await vi.advanceTimersByTimeAsync(DELAY_MS * 2)

    expect(stillDownCalls).toEqual([])
    expect(released).toEqual([])
  })

  it('重复按下（OS 自动重复）会把这一轮往后推，不会提前释放', async () => {
    const { autoRelease, released, stillDownCalls } = createHarness(() => true)

    autoRelease.press('KeyW', 'KeyW')

    for (let elapsed = 0; elapsed < DELAY_MS * 2; elapsed += 30) {
      await vi.advanceTimersByTimeAsync(30)

      autoRelease.press('KeyW', 'KeyW')
    }

    expect(stillDownCalls).toEqual([])
    expect(released).toEqual([])
  })

  it('多个原始键归一化成同一个显示名时一起问（Fn ← F5/F6，Shift ← 左右 Shift）', async () => {
    const { autoRelease, stillDownCalls } = createHarness(() => true)

    autoRelease.press('Fn', 'F5')
    autoRelease.press('Fn', 'F6')
    autoRelease.press('Shift', 'ShiftRight')

    await vi.advanceTimersByTimeAsync(DELAY_MS)

    expect(stillDownCalls).toEqual([['F5', 'F6'], ['ShiftRight']])
  })

  it('capsLock 那种「亮一下」不过问系统，到点直接释放', async () => {
    const { autoRelease, released, stillDownCalls } = createHarness(() => true)

    autoRelease.press('CapsLock', 'CapsLock', { delay: 100, probe: false })

    await vi.advanceTimersByTimeAsync(100)

    expect(stillDownCalls).toEqual([])
    expect(released).toEqual(['CapsLock'])
  })

  it('自定义延迟只管第一次等待，续轮按当时的 delay() 重新计时', async () => {
    const stillDownCalls: string[][] = []
    let delay = 500

    const autoRelease = createKeyAutoRelease({
      delay: () => delay,
      isKeyStillDown: async (rawKeys) => {
        stillDownCalls.push([...rawKeys])

        return true
      },
      onRelease: () => void 0,
    })

    // 用户在这期间把「按键自动释放延迟」改大了：续的那一轮就该按新值等
    delay = 1000

    autoRelease.press('KeyW', 'KeyW', { delay: 500 })

    await vi.advanceTimersByTimeAsync(500)
    expect(stillDownCalls).toHaveLength(1)

    await vi.advanceTimersByTimeAsync(500)
    expect(stillDownCalls).toHaveLength(1)

    await vi.advanceTimersByTimeAsync(500)
    expect(stillDownCalls).toHaveLength(2)
  })

  it('stop() 之后不再释放', async () => {
    const { autoRelease, released } = createHarness(() => true)

    autoRelease.press('KeyW', 'KeyW')
    autoRelease.stop()

    await vi.advanceTimersByTimeAsync(DELAY_MS * 3)

    expect(released).toEqual([])
  })

  /**
   * 「问系统」是异步的（一次 IPC）。这一问的工夫里这个键完全可能又抬起或又按下，
   * 迟到的答案不能再去改一个已经交出去的键。
   */
  it('探询还没回来时键又抬起了：这一轮不再插一脚', async () => {
    const released: string[] = []
    let probes = 0

    const autoRelease = createKeyAutoRelease({
      delay: () => DELAY_MS,
      isKeyStillDown: () => {
        probes += 1

        // 第一次探询慢 50ms 才回「还按着」；如果这一轮没退出去而续了第二轮，
        // 第二轮会回答「不在按着」→ 变成一次误释放
        if (probes === 1) return new Promise(resolve => setTimeout(() => resolve(true), 50))

        return Promise.resolve(false)
      },
      onRelease: key => released.push(key),
    })

    autoRelease.press('KeyW', 'KeyW')

    await vi.advanceTimersByTimeAsync(DELAY_MS)

    autoRelease.release('KeyW')

    // 迟到的「还按着」
    await vi.advanceTimersByTimeAsync(50)

    await vi.advanceTimersByTimeAsync(DELAY_MS * 2)

    expect(probes).toBe(1)
    expect(released).toEqual([])
  })
})

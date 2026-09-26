import { describe, expect, it } from 'vitest'

import { shouldSendKeepAlive } from './keepAlive'

/** 默认是「按住 KeyW、已经超过补发间隔、窗口可见」——也就是该补的那一帧 */
function check(overrides: Partial<Parameters<typeof shouldSendKeepAlive>[0]> = {}) {
  return {
    keys: ['KeyW'],
    leftDown: false,
    rightDown: false,
    sinceLastSentMs: 300,
    minIntervalMs: 250,
    pageVisible: true,
    ...overrides,
  }
}

describe('「还按着东西」时的补发判据', () => {
  it('按着键、到点了就补', () => {
    expect(shouldSendKeepAlive(check())).toBe(true)
  })

  it('还没到间隔不补（间隔就是额度闸门，补密了会白发帧）', () => {
    expect(shouldSendKeepAlive(check({ sinceLastSentMs: 249 }))).toBe(false)
  })

  it('刚好到间隔就补', () => {
    expect(shouldSendKeepAlive(check({ sinceLastSentMs: 250 }))).toBe(true)
  })

  it('什么都没按不补', () => {
    expect(shouldSendKeepAlive(check({ keys: [] }))).toBe(false)
  })

  it('只按着鼠标键也要补（对方猫会按下爪子）', () => {
    expect(shouldSendKeepAlive(check({ keys: [], leftDown: true }))).toBe(true)
    expect(shouldSendKeepAlive(check({ keys: [], rightDown: true }))).toBe(true)
  })

  /**
   * 窗口藏起来时页面会被 Chromium 冻结，`setInterval(50ms)` 被夹到 ≥1 秒，而对方 TTL 是
   * 800ms——补发反而让对方猫每秒闪一下。藏着自己看不见猫，不必补（回到改动前的表现）。
   */
  it('窗口藏起来不补', () => {
    expect(shouldSendKeepAlive(check({ pageVisible: false }))).toBe(false)
    expect(shouldSendKeepAlive(check({ keys: [], leftDown: true, pageVisible: false }))).toBe(false)
  })
})

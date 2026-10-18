import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nextTick, ref, watch } from 'vue'

import { normalizeMaxFPS, remoteFrameInterval } from './frameRate'

const state = vi.hoisted(() => ({
  shared: { maxFPS: 0 },
  apps: [] as { ticker?: { maxFPS: number, stop: () => void } }[],
  init: Promise.resolve(),
}))
vi.mock('pixi.js', () => ({
  Ticker: { shared: state.shared },
  Application: class {
    ticker?: { maxFPS: number, stop: () => void }
    constructor() {
      state.apps.push(this)
    }

    async init() {
      await state.init
      this.ticker = { maxFPS: 0, stop: vi.fn() }
    }
  },
}))
vi.mock('easy-live2d', () => ({ Config: {}, CubismSetting: vi.fn(), Live2DSprite: vi.fn(), Priority: {} }))
vi.mock('@tauri-apps/plugin-fs', () => ({ readDir: async () => [], readTextFile: vi.fn() }))
vi.mock('@/locales', () => ({ i18n: { global: { t: () => 'no model' } } }))
vi.mock('./path', () => ({ join: (...parts: string[]) => parts.join('/') }))

beforeEach(() => {
  vi.resetModules()
  state.shared.maxFPS = 0
  state.apps.length = 0
  state.init = Promise.resolve()
  vi.stubGlobal('document', { getElementById: () => null })
  vi.stubGlobal('window', {})
  vi.stubGlobal('devicePixelRatio', 1)
})

afterEach(() => vi.unstubAllGlobals())

describe('实际 watch 回调与两份 Pixi ticker 的帧率', () => {
  it('独立回调保留实例，初值和修改均生效，0 仍表示不限', async () => {
    const { default: live2d } = await import('./live2d')
    const fps = ref(30)
    const stop = watch(fps, live2d.setMaxFPS, { immediate: true })
    try {
      expect(state.shared.maxFPS).toBe(30)
      // 没有模型是此用例的预期；仍走真实 Application 初始化和限帧补写路径。
      await expect(live2d.load('test')).rejects.toThrow('no model')
      expect(state.apps[0]?.ticker?.maxFPS).toBe(30)
      fps.value = 15
      await nextTick()
      expect(state.shared.maxFPS).toBe(15)
      expect(state.apps[0]?.ticker?.maxFPS).toBe(15)
      fps.value = 0
      await nextTick()
      expect(state.shared.maxFPS).toBe(0)
      expect(state.apps[0]?.ticker?.maxFPS).toBe(0)
    } finally {
      stop()
    }
  })

  it('初始化尚未完成时改设置，最后应用最新值', async () => {
    let finish!: () => void
    state.init = new Promise<void>((resolve) => {
      finish = resolve
    })
    const { default: live2d } = await import('./live2d')
    const loading = expect(live2d.load('test')).rejects.toThrow('no model')
    live2d.setMaxFPS(30)
    live2d.setMaxFPS(10)
    finish()
    await loading
    expect(state.apps[0]?.ticker?.maxFPS).toBe(10)
  })

  it('清空和非法设置回退默认值，远端插值服从低帧率', () => {
    for (const invalid of [null, undefined, Number.NaN, Number.POSITIVE_INFINITY, -1]) {
      expect(normalizeMaxFPS(invalid)).toBe(60)
    }
    expect(remoteFrameInterval(15)).toBe(1000 / 15)
    expect(remoteFrameInterval(30)).toBe(1000 / 30)
    expect(remoteFrameInterval(0)).toBe(1000 / 60)
    expect(remoteFrameInterval(144)).toBe(1000 / 60)
  })
})

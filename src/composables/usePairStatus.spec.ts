import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { nextTick } from 'vue'

import { LISTEN_KEY } from '@/constants'
import { usePairStore } from '@/stores/pair'

import type { PairStatus } from './usePair'

const { mountedCallbacks, unmountedCallbacks, listeners, mockPairGetStatus } = vi.hoisted(() => ({
  mountedCallbacks: [] as Array<() => unknown>,
  unmountedCallbacks: [] as Array<() => unknown>,
  listeners: new Map<string, (event: { payload: unknown }) => void>(),
  mockPairGetStatus: vi.fn<() => Promise<PairStatus>>(),
}))

vi.mock('vue', async (importOriginal) => {
  const actual = await importOriginal<typeof import('vue')>()

  return {
    ...actual,
    onMounted: (callback: () => unknown) => mountedCallbacks.push(callback),
    onUnmounted: (callback: () => unknown) => unmountedCallbacks.push(callback),
  }
})

vi.mock('./usePair', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./usePair')>()

  return {
    ...actual,
    pairGetStatus: mockPairGetStatus,
  }
})

vi.mock('./useTauriListen', () => ({
  useTauriListen: (event: string, handler: (event: { payload: unknown }) => void) => {
    listeners.set(event, handler)
  },
}))

const { usePairStatus } = await import('./usePairStatus')

function status(partial: Partial<PairStatus> = {}): PairStatus {
  return {
    state: 'connected',
    p2p: 'connected',
    tier: 'full',
    tierReported: true,
    petStateHz: 60,
    plaintext: false,
    peerOnline: true,
    deviceId: 'dev-1',
    ...partial,
  }
}

describe('usePairStatus 与 enabled 开关的状态同步', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    mountedCallbacks.length = 0
    unmountedCallbacks.length = 0
    listeners.clear()
    mockPairGetStatus.mockReset()
  })

  it('pairGetStatus 先于 enabled 水合返回时，enabled 变真后立刻恢复真实连接状态且不覆盖更新后的统计', async () => {
    const store = usePairStore()

    mockPairGetStatus.mockResolvedValue(status({
      state: 'connected',
      remoteStats: {
        date: '2026-11-25',
        todayKeyboard: 100,
        todayMouse: 50,
        totalKeyboard: 200,
        totalMouse: 100,
        share: true,
      },
    }))

    usePairStatus()
    await mountedCallbacks[0]()

    expect(store.runtime.connection).toBe('disabled')
    expect(store.runtime.peerOnline).toBe(true)

    // 之后收到新的统计与暂离文案，重算 enabled 时不能把它们回滚成旧快照
    listeners.get(LISTEN_KEY.PAIR_STATS)?.({
      payload: {
        date: '2026-11-25',
        todayKeyboard: 29128,
        todayMouse: 7866,
        totalKeyboard: 129442,
        totalMouse: 46067,
        share: true,
      },
    })
    listeners.get(LISTEN_KEY.PAIR_PRESENCE)?.({
      payload: { state: 'away', message: '去倒水', displayName: '对方' },
    })

    store.settings.enabled = true
    await nextTick()

    expect(store.runtime.connection).toBe('connected')
    expect(store.runtime.p2p).toBe('connected')
    expect(store.runtime.tierReported).toBe(true)
    expect(store.runtime.petStateHz).toBe(60)
    expect(store.runtime.remoteStats?.todayKeyboard).toBe(29128)
    expect(store.runtime.remotePresenceMessage).toBe('去倒水')
  })

  it('跨窗口打开开关时，PAIR_CONNECTION_CHANGED 先于 enabled 同步到达也能自动恢复', async () => {
    const store = usePairStore()

    mockPairGetStatus.mockResolvedValue(status({ state: 'disconnected', peerOnline: false }))
    usePairStatus()

    listeners.get(LISTEN_KEY.PAIR_CONNECTION_CHANGED)?.({
      payload: status({ state: 'connected', peerOnline: true }),
    })

    expect(store.runtime.connection).toBe('disabled')

    store.settings.enabled = true
    await nextTick()

    expect(store.runtime.connection).toBe('connected')

    // 关掉开关时立刻回到 disabled 并清掉档位与直连展示
    store.settings.enabled = false
    await nextTick()

    expect(store.runtime.connection).toBe('disabled')
    expect(store.runtime.p2p).toBe('off')
    expect(store.runtime.tierReported).toBe(false)
    expect(store.runtime.petStateHz).toBe(0)
  })

  it('启动时 enabled 已经是 true，在初始查询回来之前不会停在 disabled', async () => {
    const store = usePairStore()

    store.settings.enabled = true
    mockPairGetStatus.mockReturnValue(new Promise(() => {}))

    usePairStatus()
    await nextTick()

    expect(store.runtime.connection).toBe('disconnected')
  })

  it('初始 pairGetStatus 晚于事件返回时，不会把更新后的连接状态盖回去', async () => {
    const store = usePairStore()

    store.settings.enabled = true

    let resolveInitial!: (value: PairStatus) => void
    mockPairGetStatus.mockReturnValue(new Promise((resolve) => {
      resolveInitial = resolve
    }))

    usePairStatus()
    const pending = mountedCallbacks[0]()

    listeners.get(LISTEN_KEY.PAIR_CONNECTION_CHANGED)?.({
      payload: status({ state: 'connected', peerOnline: true }),
    })

    resolveInitial(status({ state: 'disconnected', peerOnline: false }))
    await pending

    expect(store.runtime.connection).toBe('connected')
    expect(store.runtime.peerOnline).toBe(true)
  })
})

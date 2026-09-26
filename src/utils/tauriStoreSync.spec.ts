import { describe, expect, it } from 'vitest'

import { createBackendSyncGuard, nextSyncSnapshot, pickSyncedState } from './tauriStoreSync'

const LOCAL_KEYS = ['runtime', 'hasSecret', 'secretFingerprint']

/** 一个最小的 store 状态：要同步的设置 + 一堆窗口本地的运行时字段 */
function storeState(settings: Record<string, unknown> = {}, local: Record<string, unknown> = {}) {
  return {
    settings,
    runtime: { peerOnline: false, ...local },
    hasSecret: false,
    hasServerPassword: true,
    secretFingerprint: '',
  }
}

describe('「只发真的变过的设置」的判据', () => {
  it('窗口本地的键不算「要发的东西」', () => {
    expect(Object.keys(pickSyncedState(storeState(), LOCAL_KEYS)))
      .toEqual(['settings', 'hasServerPassword'])
  })

  it('还不知道后端是什么样，先发一次', () => {
    expect(nextSyncSnapshot(storeState({ enabled: true }), void 0, LOCAL_KEYS)).toEqual({
      settings: { enabled: true },
      hasServerPassword: true,
    })
  })

  it('只有运行时状态变了：不发（否则会拿着过时的设置去盖别人的）', () => {
    const last = nextSyncSnapshot(storeState({ enabled: true }), void 0, LOCAL_KEYS)

    expect(nextSyncSnapshot(storeState({ enabled: true }, { peerOnline: true }), last, LOCAL_KEYS))
      .toBeUndefined()
  })

  it('设置或服务器密码真的变了：发', () => {
    const last = nextSyncSnapshot(storeState({ enabled: true }), void 0, LOCAL_KEYS)

    expect(nextSyncSnapshot(storeState({ enabled: false }), last, LOCAL_KEYS)?.settings)
      .toEqual({ enabled: false })
    expect(nextSyncSnapshot(
      { ...storeState({ enabled: true }), hasServerPassword: false },
      last,
      LOCAL_KEYS,
    )).toEqual({ settings: { enabled: true }, hasServerPassword: false })
  })

  it('内容一样就算一样（后端过来的键序不保证，比较不能靠字符串）', () => {
    const last = nextSyncSnapshot(storeState({ enabled: true, name: 'a' }), void 0, LOCAL_KEYS)

    expect(nextSyncSnapshot(storeState({ name: 'a', enabled: true }), last, LOCAL_KEYS))
      .toBeUndefined()
  })

  it('设置里有 undefined 的键（比如还没选模型）：不会每次都判成「变了」', () => {
    const last = nextSyncSnapshot(storeState({ modelId: void 0 }), void 0, LOCAL_KEYS)

    expect(last).toEqual({ settings: {}, hasServerPassword: true })
    expect(nextSyncSnapshot(storeState({ modelId: void 0 }, { peerOnline: true }), last, LOCAL_KEYS))
      .toBeUndefined()
  })

  it('快照是脱钩的副本：之后改 store 里的设置，不会把快照一起改掉', () => {
    const settings = { scale: 100 }
    const last = nextSyncSnapshot(storeState(settings), void 0, LOCAL_KEYS)

    settings.scale = 120

    expect(nextSyncSnapshot(storeState(settings), last, LOCAL_KEYS)?.settings).toEqual({ scale: 120 })
  })
})

describe('守卫的用法（remember / sync）', () => {
  it('收到后端那份状态之后，本地状态抖动一次都不发；设置真改了才发', () => {
    const guard = createBackendSyncGuard(LOCAL_KEYS)

    // 载入：后端现在长这样
    guard.remember(storeState({ enabled: true }))

    expect(guard.sync(storeState({ enabled: true }, { peerOnline: true }))).toBeUndefined()
    expect(guard.sync(storeState({ enabled: false }))).toMatchObject({ settings: { enabled: false } })

    // 发出去之后基准跟着更新：再抖一次又不用发
    expect(guard.sync(storeState({ enabled: false }, { peerOnline: true }))).toBeUndefined()
  })

  it('没收到过后端状态也没关系：第一次变化照发', () => {
    const guard = createBackendSyncGuard(LOCAL_KEYS)

    expect(guard.sync(storeState({ enabled: true }))).toMatchObject({ settings: { enabled: true } })
  })

  it('`remember` 收到的是后端那份（本来就没有本地键），`sync` 收到的是含本地键的完整状态', () => {
    const guard = createBackendSyncGuard(LOCAL_KEYS)

    guard.remember({ settings: { enabled: true }, hasServerPassword: true })

    expect(guard.sync(storeState({ enabled: true }, { peerOnline: true }))).toBeUndefined()
  })
})

/** model store 的同一套口径：按键高亮最高 60Hz（对方猫那个窗口是对方的按键），不能跟着发 */
describe('model store 的口径', () => {
  const MODEL_LOCAL_KEYS = ['supportKeys', 'pressedKeys', 'heldKeys']

  function modelState(shortcuts: Record<string, string> = {}, pressed: Record<string, string> = {}) {
    return {
      modelReady: true,
      models: [{ id: 'a' }],
      currentModel: { id: 'a' },
      currentMotions: [],
      currentExpressions: [],
      shortcuts,
      supportKeys: { KeyA: 'left-keys/KeyA.png' },
      pressedKeys: pressed,
      heldKeys: pressed,
    }
  }

  it('只有按键高亮变了：不发', () => {
    const guard = createBackendSyncGuard(MODEL_LOCAL_KEYS)

    guard.remember(modelState({ m1: 'Ctrl+1' }))

    expect(guard.sync(modelState({ m1: 'Ctrl+1' }, { KeyA: 'left-keys/KeyA.png' }))).toBeUndefined()
  })

  it('模型或快捷键真的改了：发', () => {
    const guard = createBackendSyncGuard(MODEL_LOCAL_KEYS)

    guard.remember(modelState({ m1: 'Ctrl+1' }))

    expect(guard.sync(modelState({ m1: 'Ctrl+2' }))).toMatchObject({
      shortcuts: { m1: 'Ctrl+2' },
    })
  })
})

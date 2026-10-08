import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  store: { app: { autostart: false } },
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('@/stores/general', () => ({ useGeneralStore: () => mocks.store }))
vi.mock('@/utils/platform', () => ({ isWindows: true }))

describe('windows autostart settings', () => {
  beforeEach(() => {
    vi.resetModules()
    mocks.invoke.mockReset()
    mocks.store.app.autostart = false
  })

  it('does not save enabled until Windows confirms registration', async () => {
    let finish!: () => void
    mocks.invoke.mockReturnValue(new Promise<void>((resolve) => {
      finish = resolve
    }))
    const { useAutostart } = await import('./useAutostart')
    const state = useAutostart()
    const pending = state.apply(true)
    expect(state.busy.value).toBe(true)
    expect(mocks.store.app.autostart).toBe(false)
    await state.apply(false)
    expect(mocks.invoke).toHaveBeenCalledTimes(1)
    finish()
    await pending
    expect(mocks.store.app.autostart).toBe(true)
    expect(state.busy.value).toBe(false)
  })

  it('keeps the previous setting and displays a failure; retry is possible', async () => {
    mocks.store.app.autostart = true
    mocks.invoke.mockRejectedValueOnce('Access denied').mockResolvedValueOnce(undefined)
    const { useAutostart } = await import('./useAutostart')
    const state = useAutostart()
    await state.apply(false)
    expect(mocks.store.app.autostart).toBe(true)
    expect(state.failure.value).toBe('Access denied')
    expect(state.busy.value).toBe(false)
    await state.apply(false)
    expect(state.failure.value).toBe('')
    expect(mocks.store.app.autostart).toBe(false)
  })
})

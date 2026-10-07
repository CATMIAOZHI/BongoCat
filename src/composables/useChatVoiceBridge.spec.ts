import { beforeEach, describe, expect, it, vi } from 'vitest'
import { computed, nextTick, ref } from 'vue'

import type { VoiceDraft } from './usePair'

import { provideChatVoice, useChatVoice } from './useChatVoiceBridge'

const harness = vi.hoisted(() => ({
  mounts: [] as Array<() => unknown>,
  listeners: new Map<string, (event: { payload: unknown }) => unknown>(),
}))
vi.mock('vue', async load => ({
  ...await load<typeof import('vue')>(),
  onMounted: (callback: () => unknown) => harness.mounts.push(callback),
  onUnmounted: vi.fn(),
}))
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (event, callback) => {
    harness.listeners.set(event, callback)
    return () => harness.listeners.delete(event)
  }),
  emitTo: vi.fn(async (_target, event, payload) => {
    await harness.listeners.get(event)?.({ payload })
  }),
}))

function recorder() {
  const value = {
    recording: ref(false),
    seconds: ref(0),
    pending: ref<VoiceDraft | null>(null),
    pendingSeconds: computed(() => 3),
    playing: ref(false),
    sending: ref(false),
    error: ref(''),
    skipped: ref(false),
    discarded: ref(false),
    press: vi.fn(() => {
      value.recording.value = true
    }),
    release: vi.fn(async () => {
      value.recording.value = false
      value.pending.value = { path: 'draft.wav', durationMs: 3000 }
    }),
    play: vi.fn(async () => {
      value.playing.value = !value.playing.value
    }),
    cancel: vi.fn(() => {
      value.pending.value = null
    }),
    send: vi.fn(async () => {
      value.pending.value = null
    }),
  }
  return value
}

describe('聊天窗口共用主窗口录音', () => {
  beforeEach(() => {
    harness.mounts = []
    harness.listeners.clear()
  })

  it('同步已有草稿，结束仅待确认，试听后明确发送才调用 send', async () => {
    const source = recorder()
    provideChatVoice(source, () => '')
    const chat = useChatVoice()
    for (const mount of harness.mounts) await mount()
    expect(chat.ready.value).toBe(true)
    await chat.action('press')
    expect(chat.state.value.recording).toBe(true)
    await chat.action('release')
    expect(chat.state.value.pending).toBe(true)
    expect(source.send).not.toHaveBeenCalled()
    await chat.action('play')
    expect(chat.state.value.playing).toBe(true)
    await chat.action('send')
    expect(source.send).toHaveBeenCalledOnce()
    expect(chat.state.value.pending).toBe(false)
  })

  it('对方掉线时保留草稿并拒绝发送，重录不覆盖待确认草稿', async () => {
    const source = recorder()
    source.pending.value = { path: 'draft.wav', durationMs: 3000 }
    const reason = ref('')
    provideChatVoice(source, () => reason.value)
    const chat = useChatVoice()
    for (const mount of harness.mounts) await mount()
    expect(chat.state.value.pending).toBe(true)
    await chat.action('press')
    expect(source.press).not.toHaveBeenCalled()
    reason.value = 'pages.main.hints.sendRecordingOffline'
    await nextTick()
    await chat.action('send')
    expect(source.send).not.toHaveBeenCalled()
    expect(chat.state.value.pending).toBe(true)
    expect(chat.state.value.blockReason).toBe(reason.value)
    await chat.action('cancel')
    expect(source.cancel).toHaveBeenCalledOnce()
    expect(chat.state.value.pending).toBe(false)
  })

  it('结束录音等待中重复结束只执行一次', async () => {
    const source = recorder()
    source.recording.value = true
    let finish!: () => void
    source.release.mockImplementationOnce(() => new Promise<void>((resolve) => {
      finish = resolve
    }))
    provideChatVoice(source, () => '')
    const chat = useChatVoice()
    for (const mount of harness.mounts) await mount()
    const first = chat.action('release')
    await chat.action('release')
    expect(source.release).toHaveBeenCalledOnce()
    expect(chat.state.value.busy).toBe(true)
    finish()
    await first
    expect(chat.state.value.busy).toBe(false)
  })
})

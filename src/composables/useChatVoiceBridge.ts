import { emitTo, listen } from '@tauri-apps/api/event'
import { onMounted, onUnmounted, ref, watch } from 'vue'

import { WINDOW_LABEL } from '@/constants'

import type { usePairVoiceRecorder } from './usePairVoice'

const COMMAND = 'chat-voice-command'
const STATE = 'chat-voice-state'
type Action = 'sync' | 'press' | 'release' | 'play' | 'cancel' | 'send'
type Recorder = ReturnType<typeof usePairVoiceRecorder>
interface VoiceState {
  busy: boolean
  recording: boolean
  seconds: number
  pending: boolean
  pendingSeconds: number
  playing: boolean
  sending: boolean
  error: string
  skipped: boolean
  discarded: boolean
  blockReason: string
}

/** Main owns the only recorder; chat and keyboard shortcuts operate that same session. */
export function provideChatVoice(recorder: Recorder, blockReason: () => string) {
  const busy = ref(false)
  let dispose: (() => void) | undefined
  let disposed = false
  const snapshot = (): VoiceState => ({
    busy: busy.value,
    recording: recorder.recording.value,
    seconds: recorder.seconds.value,
    pending: Boolean(recorder.pending.value),
    pendingSeconds: recorder.pendingSeconds.value,
    playing: recorder.playing.value,
    sending: recorder.sending.value,
    error: recorder.error.value,
    skipped: recorder.skipped.value,
    discarded: recorder.discarded.value,
    blockReason: blockReason(),
  })
  const publish = () => emitTo(WINDOW_LABEL.CHAT, STATE, snapshot()).catch(() => {})

  watch(snapshot, publish)
  onMounted(async () => {
    const unlisten = await listen<Action>(COMMAND, async ({ payload }) => {
      if (payload === 'sync') {
        await publish()
        return
      }
      if (recorder.sending.value || busy.value) return
      if ((payload === 'press' || payload === 'send') && blockReason()) {
        await publish()
        return
      }
      if (payload === 'press' && recorder.pending.value) return
      if (payload === 'press' || payload === 'release' || payload === 'play'
        || payload === 'cancel' || payload === 'send') {
        busy.value = true
        try {
          await recorder[payload]()
        } finally {
          busy.value = false
          await publish()
        }
      }
    })
    if (disposed) unlisten()
    else dispose = unlisten
    await publish()
  })
  onUnmounted(() => {
    disposed = true
    dispose?.()
  })
}

export function useChatVoice() {
  const state = ref<VoiceState>({
    busy: false,
    recording: false,
    seconds: 0,
    pending: false,
    pendingSeconds: 0,
    playing: false,
    sending: false,
    error: '',
    skipped: false,
    discarded: false,
    blockReason: '',
  })
  const ready = ref(false)
  const bridgeError = ref('')
  let dispose: (() => void) | undefined
  let disposed = false

  async function action(command: Action) {
    try {
      bridgeError.value = ''
      await emitTo(WINDOW_LABEL.MAIN, COMMAND, command)
    } catch (reason) {
      bridgeError.value = String(reason)
    }
  }

  onMounted(async () => {
    const unlisten = await listen<VoiceState>(STATE, ({ payload }) => {
      state.value = payload
      ready.value = true
    })
    if (disposed) {
      unlisten()
      return
    }
    dispose = unlisten
    await action('sync')
  })
  onUnmounted(() => {
    disposed = true
    dispose?.()
  })
  return { state, ready, bridgeError, action }
}

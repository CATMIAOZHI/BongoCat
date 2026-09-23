import { onUnmounted, ref } from 'vue'

import type { ChatMessage } from './usePair'

import {
  pairCancelRecording,
  pairStartRecording,
  pairStopRecording,
  RECORDING_LIMIT_SECS,
} from './usePair'

/** 一次按住说话要调用的三个命令（§45 / §68） */
export interface VoiceActions {
  start: () => Promise<unknown>
  stop: () => Promise<ChatMessage | null>
  cancel: () => Promise<void>
}

export interface VoiceEvents {
  onError: (message: string) => void
  /** 录到了，但太短没发出去（§45 的轻点不算语音） */
  onSkipped: () => void
}

/** Rust 侧返回的是错误文案（字符串），JS 侧的异常才是 Error：两种都要能读 */
function describe(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason)
}

/**
 * 按住说话的会话（§45）：三次操作排在同一条队列上。
 *
 * 按下与松开之间可能只隔几十毫秒，而打开麦克风要花时间。松开先跑的话，
 * `pair_stop_recording` 会拿到「没在录音」，麦克风却还开着、没人去关。
 *
 * `started` 是「轻点不发送」与「麦克风打不开」的分界线：前者要提示用户，
 * 后者已经报过错了，不该再补一句「没发出去」。
 */
export function createVoiceSession(actions: VoiceActions, events: VoiceEvents) {
  let queue: Promise<void> = Promise.resolve()
  let started = false

  function chain(task: () => Promise<void>) {
    queue = queue.then(task, task)

    return queue
  }

  return {
    press() {
      return chain(async () => {
        started = false

        try {
          await actions.start()

          started = true
        } catch (reason) {
          events.onError(describe(reason))
        }
      })
    },

    release() {
      return chain(async () => {
        try {
          const message = await actions.stop()

          if (!message && started) events.onSkipped()
        } catch (reason) {
          events.onError(describe(reason))
        } finally {
          started = false
        }
      })
    },

    cancel() {
      return chain(async () => {
        try {
          await actions.cancel()
        } catch (reason) {
          events.onError(describe(reason))
        } finally {
          started = false
        }
      })
    },

    /** 队列排空（测试用） */
    settled() {
      return queue
    },
  }
}

/**
 * 「按住说话」在界面上的状态（§45）。
 *
 * 录音实体在 Rust 侧，这里只把快捷键的按下与松开翻译成命令。状态是本窗口的：
 * 录制时长的滴答、错误与「没发出去」的提示都跟着这个窗口走。
 */
export function usePairVoiceRecorder() {
  const recording = ref(false)
  const seconds = ref(0)
  /** 麦克风打不开、保存失败之类的原始错误（Rust 侧给的中文文案） */
  const error = ref('')
  /** 轻点了一下，没发送 */
  const skipped = ref(false)

  let ticker: ReturnType<typeof setInterval> | undefined
  let clearNoticeTimer: ReturnType<typeof setTimeout> | undefined
  let startedAt = 0

  function stopTicker() {
    if (!ticker) return

    clearInterval(ticker)

    ticker = void 0
  }

  function startTicker() {
    stopTicker()

    startedAt = Date.now()
    seconds.value = 0

    ticker = setInterval(() => {
      // 到上限就停在那儿：Rust 侧到点会自动收尾
      seconds.value = Math.min(Math.floor((Date.now() - startedAt) / 1000), RECORDING_LIMIT_SECS)
    }, 200)
  }

  function clearNoticeLater() {
    if (clearNoticeTimer) clearTimeout(clearNoticeTimer)

    clearNoticeTimer = setTimeout(() => {
      error.value = ''
      skipped.value = false
    }, 4000)
  }

  const session = createVoiceSession(
    {
      start: pairStartRecording,
      stop: pairStopRecording,
      cancel: pairCancelRecording,
    },
    {
      onError: (message) => {
        stopTicker()
        recording.value = false
        skipped.value = false
        error.value = message

        clearNoticeLater()
      },
      onSkipped: () => {
        error.value = ''
        skipped.value = true

        clearNoticeLater()
      },
    },
  )

  /** 快捷键按下（§45 的 Pressed）：立刻给出「正在录」的反馈，别等麦克风打开 */
  function press() {
    if (recording.value) return

    recording.value = true
    error.value = ''
    skipped.value = false

    startTicker()

    void session.press()
  }

  /** 快捷键松开（§45 的 Released）：结束录音并发送 */
  function release() {
    if (!recording.value) return

    recording.value = false

    stopTicker()

    void session.release()
  }

  /** 点录音提示上的叉：这次录音不发送 */
  function cancel() {
    if (!recording.value) return

    recording.value = false

    stopTicker()

    void session.cancel()
  }

  onUnmounted(() => {
    stopTicker()

    if (clearNoticeTimer) clearTimeout(clearNoticeTimer)
  })

  return {
    recording,
    seconds,
    error,
    skipped,
    press,
    release,
    cancel,
  }
}

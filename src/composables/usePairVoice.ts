import { convertFileSrc } from '@tauri-apps/api/core'
import { computed, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage, VoiceDraft } from './usePair'

import {
  pairCancelRecording,
  pairSendRecording,
  pairStartRecording,
  pairStopRecording,
  RECORDING_LIMIT_SECS,
} from './usePair'

/** 一次录音要调用的四个命令（§45 / §68 / R41） */
export interface VoiceActions {
  start: () => Promise<unknown>
  /** 结束录音：只把录好的 wav 交回来，**先不发送**（R41 的二次确认） */
  stop: () => Promise<VoiceDraft | null>
  /** 发送上一条待确认的录音 */
  send: () => Promise<ChatMessage>
  cancel: () => Promise<void>
}

export interface VoiceEvents {
  onError: (message: string) => void
  /** 录到了，但太短没留下来（§45 的轻点不算语音） */
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
 * `pair_stop_recording` 会拿到「没在录音」，麦克风却还开着、没人去关。发送与取消
 * （R41 新增）同理：它们都动同一份「待确认的录音」，谁先谁后不能乱。
 *
 * `started` 是「轻点不留」与「麦克风打不开」的分界线：前者要提示用户，
 * 后者已经报过错了，不该再补一句「没录上」。
 */
export function createVoiceSession(actions: VoiceActions, events: VoiceEvents) {
  let queue: Promise<void> = Promise.resolve()
  let started = false

  function chain<T>(task: () => Promise<T>) {
    const run = queue.then(task, task)

    // 队列本身永远不带着错误往下走：某一次失败不该把后面几次操作一起卡死
    queue = run.then(
      () => void 0,
      () => void 0,
    )

    return run
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

        // 交给调用方：只有麦克风真的开了才算「这次开始了」（R41 的草稿清理依赖它）
        return started
      })
    },

    release() {
      return chain(async () => {
        try {
          const draft = await actions.stop()

          // R41：停下来的录音不再直接发出去，交给界面去确认
          if (!draft) {
            if (started) events.onSkipped()

            return null
          }

          return draft
        } catch (reason) {
          events.onError(describe(reason))

          return null
        } finally {
          started = false
        }
      })
    },

    /** 发送待确认的那条录音：失败时把原因交给 `onError`，返回 `null` */
    send() {
      return chain(async () => {
        try {
          return await actions.send()
        } catch (reason) {
          events.onError(describe(reason))

          return null
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
 * 「按住说话」在界面上的状态（§45 / R41）。
 *
 * 录音实体在 Rust 侧，这里只把快捷键的按下与松开翻译成命令。状态是本窗口的：
 * 录制时长的滴答、待确认的录音、错误与「没录上」的提示都跟着这个窗口走。
 *
 * R41 的二次确认：松开（或再点一下麦克风）只把这一段 wav 录下来，界面进入「待确认」，
 * 用户可以试听、发送或取消——不再一松手就发出去。
 */
export function usePairVoiceRecorder() {
  const { t } = useI18n()

  const recording = ref(false)
  const seconds = ref(0)
  /** 录完待确认的那一条（R41）：有它时界面显示「试听 / 取消 / 发送」 */
  const pending = ref<VoiceDraft | null>(null)
  /** 正在试听上面那一条 */
  const playing = ref(false)
  /** 正在发送（发送按钮的 loading） */
  const sending = ref(false)
  /** 麦克风打不开、保存失败、发送失败之类的原始错误（多为 Rust 侧给的中文文案） */
  const error = ref('')
  /** 轻点了一下，没留下来 */
  const skipped = ref(false)
  /** 重录时把上一段待确认的录音丢掉了：要明说，别让它无声消失 */
  const discarded = ref(false)

  /** 待确认录音的秒数：用 Rust 侧算出来的真实时长，不是界面上的滴答 */
  const pendingSeconds = computed(() => {
    return pending.value ? Math.round(pending.value.durationMs / 1000) : 0
  })

  let ticker: ReturnType<typeof setInterval> | undefined
  let clearNoticeTimer: ReturnType<typeof setTimeout> | undefined
  let startedAt = 0
  let audio: HTMLAudioElement | undefined
  /**
   * 每一轮操作自增一次。
   *
   * 松开是异步的：如果结果还没回来用户又按了一次（或按了取消），那一份旧结果就不该再写回
   * 界面——否则会多出一条「点开发送报『没有等待发送的录音』」的死待确认。
   */
  let round = 0

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
      discarded.value = false
    }, 4000)
  }

  /** 停掉试听：发送、取消、重新录、窗口卸载都要走这一条 */
  function stopPlayback() {
    if (!audio) return

    audio.pause()
    audio = void 0
    playing.value = false
  }

  function failPlayback() {
    error.value = t('pages.main.hints.voicePlaybackFailed')

    clearNoticeLater()
  }

  const session = createVoiceSession(
    {
      start: pairStartRecording,
      stop: pairStopRecording,
      send: pairSendRecording,
      cancel: pairCancelRecording,
    },
    {
      onError: (message) => {
        stopTicker()
        recording.value = false
        skipped.value = false
        discarded.value = false
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

    // 让还在路上的那一次松开作废（它会把旧结果写回 pending）
    round += 1
    recording.value = true
    error.value = ''
    skipped.value = false
    discarded.value = false

    startTicker()

    // R41 / R44：上一条待确认的草稿要等**麦克风真的开了**才作废（Rust 侧同时也才删文件）；
    // 打不开麦克风时它必须原样留着，否则用户上一段录音就白丢了。
    void session.press().then((started) => {
      // 判定必须只看「start 成没成」，**不能再拿 round 比**：用户按下之后马上松开或取消时
      // round 已经变了，可 Rust 那边草稿确实已经作废，界面留着它就成了一条点不开的死待确认
      // （点试听报「读不出来」、点发送报「没有等待发送的录音」）。
      if (!started) return

      const dropped = pending.value !== null

      stopPlayback()
      pending.value = null

      // 重录会丢掉上一段：明说一句，不然它就这么无声消失了
      if (dropped) {
        error.value = ''
        skipped.value = false
        discarded.value = true

        clearNoticeLater()
      }
    })
  }

  /**
   * 快捷键松开（§45 的 Released）：结束录音，转入**待确认**（R41）。
   *
   * 录到的 wav 由 Rust 侧留在临时目录里，等用户点「发送」或「取消」。
   */
  async function release() {
    if (!recording.value) return

    const mine = ++round

    recording.value = false

    stopTicker()

    const draft = await session.release()

    if (!draft || mine !== round) return

    pending.value = draft
    // 新的一段已经录好了，「上一段已丢掉」那句就过期了
    discarded.value = false
  }

  /**
   * 取消：正在录就丢掉麦克风里那一段，已经录好待确认就删掉临时文件（R41）。
   *
   * 界面上两个取消按钮都调它，用户不用分辨自己在哪一步。
   */
  function cancel() {
    // 让还在路上的那一次松开作废：取消之后它不该再把待确认写回来
    round += 1

    stopPlayback()
    pending.value = null
    error.value = ''
    skipped.value = false
    discarded.value = false

    if (recording.value) {
      recording.value = false

      stopTicker()
    }

    void session.cancel()
  }

  /**
   * 试听刚录的这一段（R41）：再点一次就是停。
   *
   * 用 `<audio>` + asset protocol 播本机的 wav——不走网络，也不问对方。
   */
  async function play() {
    const draft = pending.value

    if (!draft) return

    // 正在发送时 Rust 侧可能已经把临时 wav 收走了：这时点试听只会冒一句「读不出来」
    if (sending.value) return

    if (playing.value) {
      stopPlayback()

      return
    }

    const element = new Audio(convertFileSrc(draft.path))

    element.onended = () => {
      if (audio === element) stopPlayback()
    }

    element.onerror = () => {
      if (audio !== element) return

      stopPlayback()
      failPlayback()
    }

    audio = element

    try {
      await element.play()

      playing.value = true
    } catch {
      stopPlayback()
      failPlayback()
    }
  }

  /**
   * 发送待确认的那一条（R41）。
   *
   * 成功与失败都不再把它留在界面上：失败时 Rust 侧已经把临时 wav 删掉了，
   * 留着只会是一个点不动的按钮。
   */
  async function send() {
    if (!pending.value || sending.value) return

    sending.value = true
    stopPlayback()

    try {
      const sent = await session.send()

      pending.value = null

      // 失败时 Rust 侧已经把临时 wav 删了，这段录音就是没了——必须说清楚「要重录」，
      // 否则用户看到「发送失败」会以为再点一次就能补发。
      if (!sent) {
        error.value = t('pages.main.hints.voiceSendFailed', { reason: error.value })

        clearNoticeLater()
      }
    } finally {
      sending.value = false
    }
  }

  onUnmounted(() => {
    stopTicker()
    stopPlayback()

    if (clearNoticeTimer) clearTimeout(clearNoticeTimer)
  })

  return {
    recording,
    seconds,
    pending,
    pendingSeconds,
    playing,
    sending,
    error,
    skipped,
    discarded,
    press,
    release,
    send,
    play,
    cancel,
  }
}

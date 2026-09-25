import { ref } from 'vue'

/**
 * 聊天窗口里播放语音（§45 / R42）。
 *
 * 全窗口共用一个播放器：切到另一条时上一条自动停，和猫咪窗口里「试听录音」用的是同一套
 * 做法（`new Audio()` + asset protocol 播本机文件，不走网络）。
 *
 * 时长不存在消息里（`AttachmentRecord` 没有这个字段），所以从音频元数据里读，读到的值按
 * 消息 id 缓存（`durations`）；正在播的那条读到之前显示 `--:--`。
 * 状态是模块级的：这个窗口只有一份播放器，多个气泡共用它。
 */
const activeId = ref('')
const playing = ref(false)
const current = ref(0)
/**
 * 每条语音读到的总时长（消息 id → 秒）。
 *
 * 按 id 存着而不是只留「当前这条」：切到另一条播放时，上一条的时长不该跟着消失。
 * 消息 id 不复用，所以这个缓存不需要在切歌时清。
 */
const durations = ref<Record<string, number>>({})
/** 播失败的是**哪一条**（只记住 id，不然一条失败会让所有语音气泡都显示「读不出来」） */
const failedId = ref('')

let audio: HTMLAudioElement | undefined

/** 摘掉当前播放器上的回调并停声：换一条、关窗口、重置历史都要走它 */
function detach() {
  if (!audio) return

  audio.ontimeupdate = null
  audio.onloadedmetadata = null
  audio.onended = null
  audio.onerror = null
  audio.pause()

  audio = void 0
}

/** 停掉播放并清空状态 */
export function stopVoicePlayback() {
  detach()

  activeId.value = ''
  playing.value = false
  current.value = 0
  failedId.value = ''
}

/** 点一下播放 / 暂停；换一条就从头开始 */
export async function toggleVoicePlayback(messageId: string, src: string) {
  if (activeId.value === messageId && audio) {
    if (playing.value) {
      audio.pause()
      playing.value = false

      return
    }

    try {
      await audio.play()

      playing.value = true
    } catch {
      fail(messageId)
    }

    return
  }

  detach()

  activeId.value = messageId
  current.value = 0
  failedId.value = ''

  const element = new Audio(src)

  element.ontimeupdate = () => {
    current.value = element.currentTime
  }

  element.onloadedmetadata = () => {
    if (!Number.isFinite(element.duration) || element.duration <= 0) return

    durations.value = { ...durations.value, [messageId]: element.duration }
  }

  element.onended = () => {
    playing.value = false
    current.value = 0
    element.currentTime = 0
  }

  element.onerror = () => {
    fail(messageId)
  }

  audio = element

  try {
    await element.play()

    playing.value = true
  } catch {
    fail(messageId)
  }
}

/** 播不出来：清掉播放状态并记住是这一条（顺序不能反，`stop` 会把 id 清空） */
function fail(messageId: string) {
  stopVoicePlayback()
  failedId.value = messageId
}

/** `0:07` 这种时长；还没读到元数据时给一个占位 */
function clock(seconds: number) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '--:--'

  const total = Math.floor(seconds)

  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`
}

export function usePairVoicePlayback() {
  return {
    isFailed: (messageId: string) => failedId.value === messageId,
    isActive: (messageId: string) => activeId.value === messageId,
    isPlaying: (messageId: string) => activeId.value === messageId && playing.value,
    percentOf: (messageId: string) => {
      const total = durations.value[messageId] ?? 0

      if (activeId.value !== messageId || !total) return 0

      return Math.min(100, Math.max(0, (current.value / total) * 100))
    },
    /**
     * 正在播的那一条显示已播时长（没播过就显示总时长）。
     *
     * 其它条只在**播放过、读到过时长**时显示总时长：每条都挂一个 `--:--` 会让人以为全都坏了。
     */
    labelOf: (messageId: string) => {
      const total = durations.value[messageId] ?? 0

      if (activeId.value !== messageId) return total ? clock(total) : ''

      return clock(playing.value || current.value ? current.value : total)
    },
    stop: stopVoicePlayback,
    toggle: toggleVoicePlayback,
  }
}

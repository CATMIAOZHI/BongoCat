import messageSound from '@/assets/audio/pair-message.wav'

/** 每个窗口各自一份音频对象（不同 WebView 不共享 DOM） */
let audio: HTMLAudioElement | undefined

/**
 * 播放新消息提示音（§46）。
 *
 * 音量 0 等价静音，直接不播。返回的 Promise 在浏览器/系统拒绝自动播放时会 reject，
 * 由调用方决定是记日志还是提示用户。
 */
export function playPairMessageSound(volume: number) {
  const level = Math.min(100, Math.max(0, Math.round(volume)))

  // 设置文件被写坏时 volume 可能是 undefined / NaN，给 media element 赋 NaN 会同步抛异常，
  // 那样调用方的 `.catch` 根本挂不上，所以这里先挡掉非有限值
  if (!Number.isFinite(level) || level <= 0) return Promise.resolve()

  audio ??= new Audio(messageSound)
  audio.volume = level / 100
  audio.currentTime = 0

  return audio.play()
}

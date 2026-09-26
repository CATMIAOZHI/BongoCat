import { invoke } from '@tauri-apps/api/core'
import { PhysicalPosition } from '@tauri-apps/api/dpi'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { isNil } from 'es-toolkit'
import { Ticker } from 'pixi.js'
import { onMounted, onUnmounted, ref, watch } from 'vue'

import type { KeyAutoReleasePressOptions } from '@/utils/keyAutoRelease'

import { useAppStore } from '@/stores/app'
import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import { inBetween } from '@/utils/is'
import { createKeyAutoRelease } from '@/utils/keyAutoRelease'
import { getCursorMonitor } from '@/utils/monitor'
import { isMac, isWindows } from '@/utils/platform'

import { CHAT_OVERLAY_RATIO, INVOKE_KEY, LISTEN_KEY, WINDOW_LABEL } from '../constants'
import { getSupportedKey as resolveSupportedKey, useModel } from './useModel'
import { usePairState } from './usePairState'
import { useTauriListen } from './useTauriListen'

interface MouseButtonEvent {
  kind: 'MousePress' | 'MouseRelease'
  value: string
}

export interface CursorPoint {
  x: number
  y: number
}

interface MouseMoveEvent {
  kind: 'MouseMove'
  value: CursorPoint
}

interface KeyboardEvent {
  kind: 'KeyboardPress' | 'KeyboardRelease'
  value: string
}

type DeviceEvent = MouseButtonEvent | MouseMoveEvent | KeyboardEvent

const DAMPING_DECAY = 0.75
/** CapsLock 只亮一下（它的「按下」是切换语义，不适合长亮，也不去问系统） */
const CAPS_LOCK_FLASH_MS = 100
/**
 * 鼠标比例夹到 0..1。
 *
 * 正常情况（点在命中显示器内）本来就落在 0..1；只有 `utils/monitor.ts` 那种兜底——
 * 点在所有显示器之外的排列空档里、按上一块显示器算——才会越界，夹住它比让模型参数越界好。
 */
const clampRatio = (value: number) => Math.min(1, Math.max(0, value))
const appWindow = getCurrentWebviewWindow()

export function useDevice() {
  const modelStore = useModelStore()
  const appStore = useAppStore()
  const catStore = useCatStore()
  const pairStore = usePairStore()
  const latestCursorPoint = ref<CursorPoint>()
  const smoothedCursorPoint = ref<CursorPoint>()
  const scaleFactor = ref(1)
  const { handlePress, handleRelease, handleMouseChange, handleMouseRatio } = useModel()
  // 本地输入同时喂给联机同步：远程猫要「哪只手 + 强度 + 比例 + 能显示的键名」（R37）
  const pairState = usePairState()

  /**
   * 问系统：这些键里有没有还按着的（R46 的「安静 ≠ 抬起」）。
   *
   * 调用失败（或键名没见过）时一律当「没有按着」，也就是退回原来的「到点就释放」，
   * 不会因为拿不到答案就让贴图一直亮着。
   */
  const isKeyStillDown = async (keys: string[]) => {
    try {
      return await invoke<boolean>(INVOKE_KEY.IS_KEY_DOWN, { keys })
    } catch {
      return false
    }
  }

  /**
   * Windows 的「按键自动释放」（见 `utils/keyAutoRelease.ts`）。
   *
   * `onStillDown` 把「系统确认还按着」告诉联机那侧：发送侧的按键也有同一个时限，
   * 不续期的话对方猫同样会在 3 秒后看不到这个键。
   */
  const keyAutoRelease = createKeyAutoRelease({
    delay: () => catStore.model.autoReleaseDelay * 1000,
    isKeyStillDown,
    onRelease: key => handleRelease(key),
    onStillDown: keys => pairState.noteKeysStillDown(keys),
  })

  const tickerCallback = (ticker: Ticker) => {
    const destination = latestCursorPoint.value

    if (!destination) return

    const current = smoothedCursorPoint.value ?? destination

    const alpha = 1 - DAMPING_DECAY ** (ticker.deltaMS / (1000 / 60))

    const interpolated = {
      x: current.x + (destination.x - current.x) * alpha,
      y: current.y + (destination.y - current.y) * alpha,
    }

    if (Math.hypot(destination.x - interpolated.x, destination.y - interpolated.y) < 0.5) {
      smoothedCursorPoint.value = { ...destination }

      latestCursorPoint.value = void 0
    } else {
      smoothedCursorPoint.value = interpolated
    }

    void handleCursorMove(smoothedCursorPoint.value)
  }

  onMounted(async () => {
    scaleFactor.value = isMac ? await appWindow.scaleFactor() : 1

    appWindow.onScaleChanged(({ payload }) => {
      if (!isMac) return

      scaleFactor.value = payload.scaleFactor
    })
  })

  onUnmounted(() => {
    Ticker.shared.remove(tickerCallback)
    keyAutoRelease.stop()
  })

  watch(() => catStore.model.ignoreMouse, (value) => {
    if (value) {
      return Ticker.shared.remove(tickerCallback)
    }

    return Ticker.shared.add(tickerCallback)
  }, { immediate: true })

  const startListening = () => {
    invoke(INVOKE_KEY.START_DEVICE_LISTENING)
  }

  const getSupportedKey = (key: string) => resolveSupportedKey(modelStore.supportKeys, key)

  const onHideOnHover = (() => {
    let timer: ReturnType<typeof setTimeout> | undefined
    let wasInWindow = false

    return (x: number, y: number) => {
      const { x: winX, y: winY, width, height } = appStore.windowState[WINDOW_LABEL.MAIN] ?? {}

      if (isNil(winX) || isNil(winY) || isNil(width) || isNil(height)) return

      const isInWindow = inBetween(x, winX, winX + width)
        && inBetween(y, winY, winY + height)

      // R39：开着双人联机时，窗口顶上那一条是聊天浮层。鼠标停在它上面不该让窗口淡出，
      // 否则「悬停就变透明」会把输入框也一起藏掉，根本点不到
      const overlayBand = pairStore.settings.enabled
        ? height * (CHAT_OVERLAY_RATIO / (1 + CHAT_OVERLAY_RATIO))
        : 0
      const isOverOverlay = overlayBand > 0 && y <= winY + overlayBand
      const shouldHide = isInWindow && !isOverOverlay

      if (shouldHide === wasInWindow) return

      if (timer) {
        clearTimeout(timer)

        timer = void 0
      }

      if (shouldHide) {
        timer = setTimeout(() => {
          document.body.style.setProperty('opacity', '0')

          appWindow.setIgnoreCursorEvents(true)
        }, catStore.window.hideOnHoverDelay * 1000)
      } else {
        document.body.style.setProperty('opacity', 'unset')

        appWindow.setIgnoreCursorEvents(catStore.window.passThrough)
      }

      wasInWindow = shouldHide
    }
  })()

  const handleCursorMove = async (cursorPoint: CursorPoint) => {
    const x = cursorPoint.x * scaleFactor.value
    const y = cursorPoint.y * scaleFactor.value

    // R12：本机渲染用平滑后的点算比例；联机同步用原始点另算一次（见下面的 syncPointer）
    const point = new PhysicalPosition(x, y)
    const monitor = await getCursorMonitor(point)

    if (monitor) {
      const { size, position } = monitor
      const xRatio = clampRatio((point.x - position.x) / size.width)
      const yRatio = clampRatio((point.y - position.y) / size.height)

      handleMouseRatio(xRatio, yRatio)
    }

    if (!catStore.window.hideOnHover) return

    onHideOnHover(x, y)
  }

  /**
   * 联机同步用的鼠标比例，直接从原始鼠标事件算。
   *
   * 以前跟着本机猫的动画帧（Ticker）一起算：本机猫窗口被隐藏时浏览器会停掉动画帧，
   * 于是鼠标位置一个都发不出去，而键盘照常同步——对方就只看到猫打字、爪子不动。
   * 远端猫自己会做插值，这里不需要平滑。开了「忽略鼠标事件」就和以前一样不发。
   */
  const syncPointer = async (cursorPoint: CursorPoint) => {
    if (catStore.model.ignoreMouse) return

    const point = new PhysicalPosition(cursorPoint.x * scaleFactor.value, cursorPoint.y * scaleFactor.value)
    const monitor = await getCursorMonitor(point)

    if (!monitor) return

    const { size, position } = monitor

    pairState.handlePointerRatio(
      clampRatio((point.x - position.x) / size.width),
      clampRatio((point.y - position.y) / size.height),
    )
  }

  /**
   * Windows：按下后等 `autoReleaseDelay` 秒（设置项），到点再向系统确认一次才当作松开了。
   *
   * 为什么不能只按时间判断：Windows 的键盘自动重复只跟**最后按下**的那个键，按住 w 再按
   * a/d 之后 w 会完全安静（松开 a/d 也不会恢复重复），于是「多久没事件」这件事根本说明不了
   * 它抬没抬起——只看时间就会把一直按着的 w 当成松开，高亮就没了（R46）。
   */
  const handleAutoRelease = (key: string, raw: string, pressOptions?: KeyAutoReleasePressOptions) => {
    handlePress(key)
    keyAutoRelease.press(key, raw, pressOptions)
  }

  /** 真的收到了抬起事件：先把待发的自动释放撤掉（少一次多余的判断），再松开显示 */
  const handleAutoReleaseRelease = (key: string) => {
    keyAutoRelease.release(key)
    handleRelease(key)
  }

  useTauriListen<DeviceEvent>(LISTEN_KEY.DEVICE_CHANGED, ({ payload }) => {
    const { kind, value } = payload

    if (kind === 'KeyboardPress' || kind === 'KeyboardRelease') {
      // R2：左右手判定必须用 rdev 的原始键名，不能先过 getSupportedKey 的归一化
      pairState.handleKeyboard(value, kind === 'KeyboardPress')

      const nextValue = getSupportedKey(value)

      if (!nextValue) return

      if (nextValue === 'CapsLock') {
        if (kind === 'KeyboardPress') {
          return handleAutoRelease(nextValue, value, { delay: CAPS_LOCK_FLASH_MS, probe: false })
        }

        return handleAutoReleaseRelease(nextValue)
      }

      if (kind === 'KeyboardPress') {
        // Windows 下部分系统级按键收不到抬起事件，所以走「到点再确认」这条路（R46）
        if (isWindows) return handleAutoRelease(nextValue, value)

        return handlePress(nextValue)
      }

      if (isWindows) return handleAutoReleaseRelease(nextValue)

      return handleRelease(nextValue)
    }

    switch (kind) {
      case 'MousePress':
        pairState.handleMouseButton(value, true)

        return handleMouseChange(value)
      case 'MouseRelease':
        pairState.handleMouseButton(value, false)

        return handleMouseChange(value, false)
      case 'MouseMove':
        pairState.handlePointerMove(value)
        void syncPointer(value)

        return latestCursorPoint.value = value
    }
  })

  return {
    startListening,
  }
}

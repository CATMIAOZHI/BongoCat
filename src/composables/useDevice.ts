import { invoke } from '@tauri-apps/api/core'
import { PhysicalPosition } from '@tauri-apps/api/dpi'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { isNil } from 'es-toolkit'
import { Ticker } from 'pixi.js'
import { onMounted, onUnmounted, ref, watch } from 'vue'

import { useAppStore } from '@/stores/app'
import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import { inBetween } from '@/utils/is'
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
const appWindow = getCurrentWebviewWindow()

export function useDevice() {
  const modelStore = useModelStore()
  const releaseTimers = new Map<string, NodeJS.Timeout>()
  const appStore = useAppStore()
  const catStore = useCatStore()
  const pairStore = usePairStore()
  const latestCursorPoint = ref<CursorPoint>()
  const smoothedCursorPoint = ref<CursorPoint>()
  const scaleFactor = ref(1)
  const { handlePress, handleRelease, handleMouseChange, handleMouseRatio } = useModel()
  // 本地输入同时喂给联机同步：远程猫要「哪只手 + 强度 + 比例 + 能显示的键名」（R37）
  const pairState = usePairState()

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

    // R12：屏幕比例只在这里算一次，本地渲染与联机同步共用同一个值
    const point = new PhysicalPosition(x, y)
    const monitor = await getCursorMonitor(point)

    if (monitor) {
      const { size, position } = monitor
      const xRatio = (point.x - position.x) / size.width
      const yRatio = (point.y - position.y) / size.height

      handleMouseRatio(xRatio, yRatio)
      pairState.handlePointerRatio(xRatio, yRatio)
    }

    if (!catStore.window.hideOnHover) return

    onHideOnHover(x, y)
  }

  const handleAutoRelease = (key: string, delay = 100) => {
    handlePress(key)

    if (releaseTimers.has(key)) {
      clearTimeout(releaseTimers.get(key))
    }

    const timer = setTimeout(() => {
      handleRelease(key)

      releaseTimers.delete(key)
    }, delay)

    releaseTimers.set(key, timer)
  }

  useTauriListen<DeviceEvent>(LISTEN_KEY.DEVICE_CHANGED, ({ payload }) => {
    const { kind, value } = payload

    // R39：正在猫咪窗口的聊天浮层里打字。这段输入属于「在写消息」，不是「在敲猫」：
    // 本机不按贴图、也不发给对方（鼠标位置除外，让对方的猫继续跟着指针看）
    if (pairStore.runtime.localInputPaused && kind !== 'MouseMove') return

    if (kind === 'KeyboardPress' || kind === 'KeyboardRelease') {
      // R2：左右手判定必须用 rdev 的原始键名，不能先过 getSupportedKey 的归一化
      pairState.handleKeyboard(value, kind === 'KeyboardPress')

      const nextValue = getSupportedKey(value)

      if (!nextValue) return

      if (nextValue === 'CapsLock') {
        return handleAutoRelease(nextValue)
      }

      if (kind === 'KeyboardPress') {
        if (isWindows) {
          const delay = catStore.model.autoReleaseDelay * 1000

          return handleAutoRelease(nextValue, delay)
        }

        return handlePress(nextValue)
      }

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

        return latestCursorPoint.value = value
    }
  })

  return {
    startListening,
  }
}

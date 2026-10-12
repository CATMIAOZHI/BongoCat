import type { Event } from '@tauri-apps/api/event'

import { PhysicalPosition, PhysicalSize } from '@tauri-apps/api/dpi'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { availableMonitors } from '@tauri-apps/api/window'
import { useDebounceFn } from '@vueuse/core'
import { isNumber } from 'es-toolkit/compat'
import { onMounted, ref, watch } from 'vue'

import { WINDOW_LABEL } from '@/constants'
import { useAppStore } from '@/stores/app'
import { useCatStore } from '@/stores/cat'
import { clampIntoMonitor, pickMonitor } from '@/utils/monitor'

export type WindowState = Record<string, Partial<PhysicalPosition & PhysicalSize> | undefined>

const appWindow = getCurrentWebviewWindow()
const { label } = appWindow

export function useWindowState() {
  const appStore = useAppStore()
  const catStore = useCatStore()
  const isRestored = ref(false)

  onMounted(() => {
    appWindow.onMoved(onChange)

    appWindow.onResized(onChange)

    appWindow.onScaleChanged(clampToMonitor)
  })

  /**
   * 哪几扇窗要「保持在屏幕内」：自己的猫与对方猫。
   *
   * 对方猫没有自己的开关，跟着同一个偏好走——两只猫拖起来的手感得一致，不能自己的猫拖不出
   * 屏幕、对方猫却能丢到屏幕外（它是贴桌面的浮层，丢了就只能去设置页里改位置）。
   * 聊天窗口不夹：那是个随手拖到哪都行的普通窗口。
   */
  const shouldKeepInScreen = () => {
    if (label !== WINDOW_LABEL.MAIN && label !== WINDOW_LABEL.REMOTE_CAT) return false

    return catStore.window.keepInScreen
  }

  const clampToMonitor = useDebounceFn(async () => {
    if (!shouldKeepInScreen()) return

    const monitors = await availableMonitors()

    if (!monitors.length) return

    const size = await appWindow.outerSize()
    const position = await appWindow.outerPosition()

    const monitor = pickMonitor({ position, size }, monitors)

    if (!monitor) return

    const { x, y } = clampIntoMonitor({ position, size }, monitor)

    if (x === position.x && y === position.y) return

    return appWindow.setPosition(new PhysicalPosition(x, y))
  }, 500)

  watch(() => catStore.window.keepInScreen, clampToMonitor)

  const onChange = async (event: Event<PhysicalPosition | PhysicalSize>) => {
    // 恢复位置之前不记账：窗口刚建出来时系统给的那个默认位置（主屏）也会发一次 Moved，
    // 那一次写进 store 就把存档里的位置盖掉了，重启后猫就落在主屏上。
    if (!isRestored.value) return

    const minimized = await appWindow.isMinimized()

    if (minimized) return

    appStore.windowState[label] ??= {}

    Object.assign(appStore.windowState[label], event.payload)

    clampToMonitor()
  }

  const restoreState = async () => {
    const { x, y, width, height } = appStore.windowState[label] ?? {}
    const applied: Record<string, number> = {}

    // 先定尺寸再定位置：夹取要用最终尺寸算（窗口越大，能放的位置越靠左上）
    if (width && height) {
      await appWindow.setSize(new PhysicalSize(width, height))

      applied.width = width
      applied.height = height
    }

    if (isNumber(x) && isNumber(y)) {
      const size = await appWindow.outerSize()
      const rect = { position: new PhysicalPosition(x, y), size }

      // 要「保持在屏幕内」的窗口（两只猫）按同一条规则夹进它落在的那块屏；其余窗口照存档原样
      // 恢复。以前是「落点正好在某块显示器里才恢复」，认不到就整段不应用——那块屏没接、或者
      // 分辨率 / 排列变了，窗口就掉回默认位置（主屏）。
      const monitor = shouldKeepInScreen() ? pickMonitor(rect, await availableMonitors()) : null
      const position = monitor ? clampIntoMonitor(rect, monitor) : rect.position

      await appWindow.setPosition(new PhysicalPosition(position.x, position.y))

      applied.x = position.x
      applied.y = position.y
    }

    // 恢复自己产生的那次 Moved / Resized 会被上面那道守卫丢掉（那时 isRestored 还是 false），
    // 所以把真正落地的值补进 store——别的窗口与本窗的鼠标比例换算都读它。
    appStore.windowState[label] ??= {}

    Object.assign(appStore.windowState[label], applied)

    isRestored.value = true

    clampToMonitor()
  }

  return {
    isRestored,
    restoreState,
  }
}

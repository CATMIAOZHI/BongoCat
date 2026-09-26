import type { Monitor, PhysicalPosition } from '@tauri-apps/api/window'

import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { cursorPosition, monitorFromPoint, primaryMonitor } from '@tauri-apps/api/window'

import { isMac } from './platform'

/**
 * 查显示器时该用哪个坐标空间（纯函数，见 `monitor.spec.ts`）。
 *
 * `monitorFromPoint(x, y)` 的坐标是**原样**交给 tao 的：Windows 上 tao 直接把它喂给
 * `MonitorFromPoint`（物理像素 + `MONITOR_DEFAULTTONULL`），macOS 上才是逻辑点。
 * 所以非 macOS 必须传物理点——以前一律 `toLogical(scaleFactor)`，在非 100% 缩放下等于拿一个
 * 缩小过的点去查，跨屏附近会认成隔壁那块显示器。
 */
export function monitorQueryPoint(point: PhysicalPosition, scaleFactor: number) {
  return isMac ? point.toLogical(scaleFactor) : { x: point.x, y: point.y }
}

/**
 * 查不到显示器时的兜底（见 `monitor.spec.ts`）。
 *
 * 查不到**不能**把这一帧丢掉：调用方拿到 `null` 就整个放弃更新鼠标比例，猫咪的爪子当场冻住。
 * 多屏排列留下的空档（两块显示器高度不一致时，高出来的那一侧没人覆盖的带子）正好就在
 * 「鼠标贴着屏幕边缘滑动」的位置上。退回上一次认到的显示器最贴近实际——多半就是鼠标刚离开
 * 的那块；一次都没认到才退回主显示器（传函数进来，前面命中就不必白问一次）。
 */
export async function chooseMonitor(
  found: Monitor | null,
  cached: Monitor | null,
  primary: () => Promise<Monitor | null>,
): Promise<Monitor | null> {
  return found ?? cached ?? await primary()
}

function createCursorMonitor() {
  let cachedMonitor: Monitor | null = null

  return async (cursorPoint?: PhysicalPosition) => {
    cursorPoint ??= await cursorPosition()

    if (cachedMonitor) {
      const { size, position } = cachedMonitor

      const inBounds = cursorPoint.x >= position.x
        && cursorPoint.x < position.x + size.width
        && cursorPoint.y >= position.y
        && cursorPoint.y < position.y + size.height

      if (inBounds) {
        return cachedMonitor
      }
    }

    const appWindow = getCurrentWebviewWindow()

    const scaleFactor = await appWindow.scaleFactor()

    const { x, y } = monitorQueryPoint(cursorPoint, scaleFactor)

    const found = await monitorFromPoint(x, y)

    cachedMonitor = await chooseMonitor(found, cachedMonitor, primaryMonitor)

    return cachedMonitor
  }
}

export const getCursorMonitor = createCursorMonitor()

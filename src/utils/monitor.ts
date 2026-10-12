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

/** 窗口 / 显示器矩形的公共形状（`PhysicalPosition` 与 `PhysicalSize` 都符合） */
export interface RectLike {
  position: { x: number, y: number }
  size: { width: number, height: number }
}

/** 两个矩形重叠的面积，没重叠就是 0 */
function overlapArea(rect: RectLike, target: RectLike) {
  const left = Math.max(rect.position.x, target.position.x)
  const top = Math.max(rect.position.y, target.position.y)
  const right = Math.min(rect.position.x + rect.size.width, target.position.x + target.size.width)
  const bottom = Math.min(rect.position.y + rect.size.height, target.position.y + target.size.height)

  return Math.max(0, right - left) * Math.max(0, bottom - top)
}

/**
 * 两个矩形之间的距离（按矩形边缘算，不是中心点）。
 *
 * 中心点会在窗口比空档还大时骗人：那时窗口中心离两块屏可能一样远，而边缘上明明已经贴住
 * 其中一块了。边缘距离在这种形状上给出的答案更贴近「用户把它丢在哪边」。
 */
function gapDistance(rect: RectLike, target: RectLike) {
  const dx = Math.max(
    target.position.x - (rect.position.x + rect.size.width),
    rect.position.x - (target.position.x + target.size.width),
  )
  const dy = Math.max(
    target.position.y - (rect.position.y + rect.size.height),
    rect.position.y - (target.position.y + target.size.height),
  )

  return Math.hypot(Math.max(0, dx), Math.max(0, dy))
}

/**
 * 选出窗口该被夹到哪块显示器（纯函数，见 `monitor.spec.ts`）。
 *
 * 判据是**窗口自己覆盖面积最大**的那块，不是鼠标所在的那块：夹取会在「最后一次移动之后」
 * 才跑（调用方带着 500ms 防抖），那时鼠标的位置只说明用户的手在哪，窗口才算「落在哪块屏」。
 * 用鼠标那一版的实际后果是两个都报过的问题：重启后鼠标多半停在主屏（猫被夹回主屏 = 位置乱
 * 跑），以及双屏高度不一致时鼠标一进右边那块、夹取目标就换屏（看起来「右边能拖出屏幕、
 * 上面拖不动」）。
 *
 * 一块都没覆盖时（两块显示器高度/排列不同会留下谁都没盖住的空档）退回**最近**的那块，
 * 不能返回 null：返回 null 会让窗口停在谁都不认的地方，看着就像被丢出屏幕了。
 */
export function pickMonitor(rect: RectLike, monitors: Monitor[]): Monitor | null {
  let covered: Monitor | null = null
  let coveredArea = 0

  for (const monitor of monitors) {
    const area = overlapArea(rect, monitor)

    if (area > coveredArea) {
      covered = monitor
      coveredArea = area
    }
  }

  if (covered) return covered

  let nearest: Monitor | null = null
  let nearestDistance = Number.POSITIVE_INFINITY

  for (const monitor of monitors) {
    const distance = gapDistance(rect, monitor)

    if (distance < nearestDistance) {
      nearest = monitor
      nearestDistance = distance
    }
  }

  return nearest
}

/**
 * 把窗口位置夹进某块显示器（纯函数，见 `monitor.spec.ts`）。
 *
 * 窗口比显示器还大时两个边界会交叉（`maxX < minX`）：`Math.min` 先给出 `maxX`，`Math.max`
 * 再把它钉回显示器原点。顺序与改动前一致，别把两步并成一个 `Math.min(Math.max(...))`。
 */
export function clampIntoMonitor(rect: RectLike, monitor: RectLike) {
  const maxX = monitor.position.x + monitor.size.width - rect.size.width
  const maxY = monitor.position.y + monitor.size.height - rect.size.height

  return {
    x: Math.max(monitor.position.x, Math.min(rect.position.x, maxX)),
    y: Math.max(monitor.position.y, Math.min(rect.position.y, maxY)),
  }
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

    const scaleFactor = isMac ? await appWindow.scaleFactor() : 1

    const { x, y } = monitorQueryPoint(cursorPoint, scaleFactor)

    const found = await monitorFromPoint(x, y)

    cachedMonitor = await chooseMonitor(found, cachedMonitor, primaryMonitor)

    return cachedMonitor
  }
}

export const getCursorMonitor = createCursorMonitor()

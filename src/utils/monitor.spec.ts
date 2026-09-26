import type { Monitor } from '@tauri-apps/api/window'

import { PhysicalPosition, PhysicalSize } from '@tauri-apps/api/dpi'
import { describe, expect, it, vi } from 'vitest'

import { chooseMonitor, monitorQueryPoint } from './monitor'

// `utils/platform.ts` 在模块加载时就要问一次 Tauri（`platform()`），node 里跑不了；
// 这里换成固定值，顺带把「按平台分坐标空间」这件事钉住
vi.mock('./platform', () => ({ isLinux: false, isMac: false, isWindows: true }))

function monitor(scaleFactor: number): Monitor {
  const size = new PhysicalSize(1920, 1080)
  const position = new PhysicalPosition(0, 0)

  return {
    name: 'display',
    position,
    size,
    scaleFactor,
    workArea: { position, size },
  }
}

describe('查鼠标所在显示器的口径', () => {
  /**
   * `monitorFromPoint` 的坐标原样交给 tao：Windows 上 tao 拿它去 `MonitorFromPoint`
   * （物理像素），macOS 上才当逻辑点。传错空间时，非 100% 缩放会拿缩小过的点去查，
   * 跨屏附近认成隔壁那块显示器。（macOS 那条分支就是改动前的行为，没动。）
   */
  it('windows 原样传物理点，不按缩放折算', () => {
    const point = new PhysicalPosition(2880, 1620)

    expect(monitorQueryPoint(point, 1.5)).toEqual({ x: 2880, y: 1620 })
    expect(monitorQueryPoint(point, 1)).toEqual({ x: 2880, y: 1620 })
  })

  /**
   * 查不到显示器时不能返回 null：调用方拿到 null 会直接跳过这一帧，猫咪的爪子冻住。
   * 多屏高度不一致留下的空档正好在屏幕边缘上（用户报的「贴着屏幕边缘滑动时不更新」）。
   */
  it('查不到就退回上一次认到的显示器，其次主显示器', async () => {
    const cached = monitor(1)
    const primary = monitor(1.25)
    const found = monitor(2)

    expect(await chooseMonitor(found, cached, () => Promise.resolve(primary))).toBe(found)
    expect(await chooseMonitor(null, cached, () => Promise.resolve(primary))).toBe(cached)
    expect(await chooseMonitor(null, null, () => Promise.resolve(primary))).toBe(primary)
    expect(await chooseMonitor(null, null, () => Promise.resolve(null))).toBeNull()
  })

  /** 主显示器是最后才问的：前面命中就不该多打一次 IPC（查光标这条路上每帧都跑） */
  it('命中时不问主显示器', async () => {
    const askPrimary = vi.fn(() => Promise.resolve(monitor(1)))

    await chooseMonitor(monitor(2), monitor(1), askPrimary)
    await chooseMonitor(null, monitor(1), askPrimary)
    await chooseMonitor(null, null, askPrimary)

    expect(askPrimary).toHaveBeenCalledTimes(1)
  })
})

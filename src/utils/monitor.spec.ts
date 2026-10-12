import type { Monitor } from '@tauri-apps/api/window'

import { PhysicalPosition, PhysicalSize } from '@tauri-apps/api/dpi'
import { describe, expect, it, vi } from 'vitest'

import { chooseMonitor, clampIntoMonitor, monitorQueryPoint, pickMonitor } from './monitor'

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

function at(x: number, y: number, width: number, height: number): Monitor {
  const size = new PhysicalSize(width, height)
  const position = new PhysicalPosition(x, y)

  return {
    name: `display-${x}-${y}`,
    position,
    size,
    scaleFactor: 1,
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

// 用户的双屏就是这种形状：主屏 2133×1600 在左上，右边那块 2560×1440 从 y=149 开始，
// 于是 x>2133 且 y<149 那一带谁都没盖住（「空档」）。所有夹取判据都按物理像素走。
const primary = at(0, 0, 2133, 1600)
const secondary = at(2133, 149, 2560, 1440)

function rect(x: number, y: number, width = 257, height = 149) {
  return {
    position: new PhysicalPosition(x, y),
    size: new PhysicalSize(width, height),
  }
}

describe('窗口该被夹到哪块显示器', () => {
  /**
   * 判据必须是「窗口覆盖最多」而不是「鼠标在哪」：夹取在最后一次移动之后才跑，那时鼠标只说明
   * 手在哪。用鼠标那一版的实际后果就是「重启后鼠标停在主屏 → 猫被夹回主屏」（位置乱跑）。
   */
  it('按窗口覆盖面积选，不按鼠标', () => {
    expect(pickMonitor(rect(100, 200), [primary, secondary])).toBe(primary)
    expect(pickMonitor(rect(4000, 300), [primary, secondary])).toBe(secondary)

    // 跨在两块屏之间：左边露出 100px、右边露出 157px，按右边那块算
    expect(pickMonitor(rect(2033, 300), [primary, secondary])).toBe(secondary)
    // 反过来，左边露出 200px，按左边那块算
    expect(pickMonitor(rect(1933, 300), [primary, secondary])).toBe(primary)
  })

  /** 空档里不能返回 null：返回 null 会让窗口停在谁都不认的地方，看着就像被丢出屏幕了 */
  it('掉进两块屏之间的空档时退回最近的一块', () => {
    expect(pickMonitor(rect(2400, -60), [primary, secondary])).toBe(secondary)
    expect(pickMonitor(rect(-800, 100), [primary, secondary])).toBe(primary)
  })

  it('没有显示器时返回 null', () => {
    expect(pickMonitor(rect(0, 0), [])).toBeNull()
  })
})

describe('把窗口位置夹进显示器', () => {
  it('超出右边 / 下边会拉回来，超出的左边 / 上边也会推回去', () => {
    expect(clampIntoMonitor(rect(4500, 300), secondary)).toEqual({ x: 4436, y: 300 })
    expect(clampIntoMonitor(rect(4500, 1500), secondary)).toEqual({ x: 4436, y: 1440 })
    expect(clampIntoMonitor(rect(2100, 100), secondary)).toEqual({ x: 2133, y: 149 })
  })

  it('本来就完全在某块屏里就不动', () => {
    expect(clampIntoMonitor(rect(4417, 166), secondary)).toEqual({ x: 4417, y: 166 })
    expect(clampIntoMonitor(rect(100, 100), primary)).toEqual({ x: 100, y: 100 })
  })

  /** 窗口比显示器还大（缩放拉到很大）：钉在显示器原点上，而不是算出一个反方向的坐标 */
  it('窗口比显示器还大时钉在显示器原点', () => {
    expect(clampIntoMonitor(rect(3000, 300, 3000, 2000), secondary)).toEqual({ x: 2133, y: 149 })
  })
})

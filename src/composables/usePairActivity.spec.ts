import { describe, expect, it } from 'vitest'

import type { PetSnapshot } from './usePairActivity'

import {
  countStats,
  createPairActivityMapper,
  KEY_LIST_MAX,
  POINTER_IDLE_MS,
  quantize,
  rolloverStats,
  sanitizeKeys,
  TYPING_WINDOW_MS,
} from './usePairActivity'

describe('宠物快照的口径', () => {
  it('载荷里不出现真实像素坐标，只有 0..1 的比例', () => {
    const mapper = createPairActivityMapper()
    const now = 2_000

    // 1920x1080 屏幕上位于 (1500, 900) 的鼠标
    mapper.handlePointerRatio(1500 / 1920, 900 / 1080, now)

    const payload = JSON.stringify(mapper.snapshot(now))

    expect(payload).not.toContain('1500')
    expect(payload).not.toContain('1080')
    expect(payload).not.toContain('900')

    for (const value of Object.values(mapper.snapshot(now).pointer)) {
      if (typeof value === 'number') {
        expect(value).toBeGreaterThanOrEqual(0)
        expect(value).toBeLessThanOrEqual(1)
      }
    }
  })

  it('越界与非法值都会被裁掉', () => {
    const mapper = createPairActivityMapper()

    mapper.handlePointerRatio(2, 4, 0)

    expect(mapper.snapshot(0).pointer.x).toBe(1)
    expect(mapper.snapshot(0).pointer.y).toBe(1)

    // 非法样本直接丢弃，不会把比例写成 NaN
    mapper.reset()
    mapper.handlePointerRatio(Number.NaN, 0.5, 0)

    expect(mapper.snapshot(0).pointer.x).toBe(0.5)
    expect(quantize(Number.POSITIVE_INFINITY, 0.2)).toBe(0)
  })
})

describe('r37 键名同步', () => {
  it('按着的键名会进载荷，但只带本机模型能显示的那些', () => {
    const mapper = createPairActivityMapper({ isSupportedKey: key => key !== 'F5' })

    mapper.handleKeyboard('KeyA', true, 0)
    mapper.handleKeyboard('F5', true, 0)
    mapper.handleKeyboard('ShiftLeft', true, 0)

    expect(mapper.snapshot(0).keyboard.keys).toEqual(['KeyA', 'ShiftLeft'])

    mapper.handleKeyboard('KeyA', false, 10)

    expect(mapper.snapshot(10).keyboard.keys).toEqual(['ShiftLeft'])
  })

  it('键名去重、排序、封顶', () => {
    const mapper = createPairActivityMapper()

    for (const key of ['KeyD', 'KeyC', 'KeyB', 'KeyA', 'KeyJ', 'KeyI', 'KeyH', 'KeyG', 'KeyF', 'KeyE']) {
      mapper.handleKeyboard(key, true, 0)
    }

    const { keys } = mapper.snapshot(0).keyboard

    expect(keys).toHaveLength(KEY_LIST_MAX)
    expect(keys).toEqual([...keys].sort())
  })

  it('收不到释放事件的键会被按住上限兜住', () => {
    const mapper = createPairActivityMapper({ handHoldLimitMs: () => 1_000 })

    mapper.handleKeyboard('KeyA', true, 0)

    expect(mapper.snapshot(500).keyboard.keys).toEqual(['KeyA'])
    expect(mapper.snapshot(1_500).keyboard.keys).toEqual([])
  })

  it('对端发来的畸形键名会被清洗掉', () => {
    expect(sanitizeKeys(['KeyA', 'KeyA', 'Key B', 'a'.repeat(64), '', 42, null])).toEqual(['KeyA'])
    expect(sanitizeKeys('KeyA')).toEqual([])
    expect(sanitizeKeys(Array.from({ length: 20 }, (_, index) => `Key${index}`))).toHaveLength(KEY_LIST_MAX)
  })
})

describe('r2 左右手分区', () => {
  const handOf = (key: string) => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard(key, true, 0)

    const { keyboard } = mapper.snapshot(0)

    return { left: keyboard.leftHand, right: keyboard.rightHand }
  }

  it('按物理键位判定左右手', () => {
    expect(handOf('KeyA')).toEqual({ left: true, right: false })
    expect(handOf('KeyQ')).toEqual({ left: true, right: false })
    expect(handOf('KeyB')).toEqual({ left: true, right: false })
    expect(handOf('Num3')).toEqual({ left: true, right: false })
    expect(handOf('Space')).toEqual({ left: true, right: false })

    expect(handOf('KeyL')).toEqual({ left: false, right: true })
    expect(handOf('KeyY')).toEqual({ left: false, right: true })
    expect(handOf('Num0')).toEqual({ left: false, right: true })
    expect(handOf('Return')).toEqual({ left: false, right: true })
    // 方向键用 rdev 的原始键名（`UpArrow`），与模型目录 resources/right-keys 里的贴图同名
    expect(handOf('UpArrow')).toEqual({ left: false, right: true })
    expect(handOf('DownArrow')).toEqual({ left: false, right: true })
    expect(handOf('LeftArrow')).toEqual({ left: false, right: true })
    expect(handOf('RightArrow')).toEqual({ left: false, right: true })
    expect(handOf('Kp4')).toEqual({ left: false, right: true })
  })

  it('修饰键算手，但不算打字强度', () => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard('ShiftLeft', true, 0)
    mapper.handleKeyboard('ControlRight', true, 0)

    const { keyboard } = mapper.snapshot(0)

    expect(keyboard.leftHand).toBe(true)
    expect(keyboard.rightHand).toBe(true)
    expect(keyboard.intensity).toBe(0)
  })

  it('未知按键不进左右手', () => {
    expect(handOf('Unknown(99)')).toEqual({ left: false, right: false })
  })

  it('抬起之后手就放下', () => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard('KeyA', true, 0)
    expect(mapper.snapshot(10).keyboard.leftHand).toBe(true)

    mapper.handleKeyboard('KeyA', false, 20)
    expect(mapper.snapshot(20).keyboard.leftHand).toBe(false)
  })

  it('按住上限只影响显示，不影响去重', () => {
    // Windows 上部分系统键收不到释放事件，靠这个上限兜底
    const mapper = createPairActivityMapper({ handHoldLimitMs: () => 1_000 })

    expect(mapper.handleKeyboard('KeyA', true, 0)).toBe('pressed')
    expect(mapper.snapshot(500).keyboard.leftHand).toBe(true)
    expect(mapper.snapshot(1_500).keyboard.leftHand).toBe(false)

    // 键本身仍在按下集合里，所以 OS 自动重复不会被算成新的按下
    expect(mapper.handleKeyboard('KeyA', true, 1_600)).toBe('repeat')
  })
})

describe('r3 打字强度', () => {
  const intensityAfter = (keys: string[], now: number) => {
    const mapper = createPairActivityMapper()

    keys.forEach((key, index) => mapper.handleKeyboard(key, true, index))

    return mapper.snapshot(now).keyboard.intensity
  }

  it('档位是 0 / 0.2 / 0.4 / 0.6 / 0.8 / 1.0', () => {
    expect(intensityAfter([], 0)).toBe(0)
    expect(intensityAfter(['KeyA'], 10)).toBe(0.2)
    expect(intensityAfter(['KeyA', 'KeyB'], 10)).toBe(0.4)
    expect(intensityAfter(['KeyA', 'KeyB', 'KeyC'], 10)).toBe(0.6)
    expect(intensityAfter(['KeyA', 'KeyB', 'KeyC', 'KeyD'], 10)).toBe(0.8)
    expect(intensityAfter(['KeyA', 'KeyB', 'KeyC', 'KeyD', 'KeyE'], 10)).toBe(1)
    expect(intensityAfter(['KeyA', 'KeyB', 'KeyC', 'KeyD', 'KeyE', 'KeyF'], 10)).toBe(1)
  })

  it('同一个键按住不放只算一次', () => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard('KeyA', true, 0)
    mapper.handleKeyboard('KeyA', true, 50)
    mapper.handleKeyboard('KeyA', true, 100)

    expect(mapper.snapshot(100).keyboard.intensity).toBe(0.2)
  })

  it('超过 500ms 窗口后衰减回 0', () => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard('KeyA', true, 0)
    expect(mapper.snapshot(TYPING_WINDOW_MS + 1).keyboard.intensity).toBe(0)
  })
})

describe('r4 量化与鼠标状态', () => {
  it('比例按 0.02、强度按 0.2 取整', () => {
    expect(quantize(0.337, 0.02)).toBe(0.34)
    expect(quantize(0.994, 0.02)).toBe(1)
    expect(quantize(0.66, 0.2)).toBe(0.6)
  })

  it('左右键状态跟随鼠标按键', () => {
    const mapper = createPairActivityMapper()

    mapper.handleMouseButton('Left', true)

    expect(mapper.snapshot(0).pointer).toMatchObject({ leftDown: true, rightDown: false })

    mapper.handleMouseButton('Right', true)
    mapper.handleMouseButton('Left', false)

    expect(mapper.snapshot(0).pointer).toMatchObject({ leftDown: false, rightDown: true })
  })

  it('太久没移动就不算活跃', () => {
    const mapper = createPairActivityMapper()

    mapper.handlePointerRatio(0.3, 0.3, 0)

    expect(mapper.snapshot(10).pointer.active).toBe(true)
    expect(mapper.snapshot(POINTER_IDLE_MS + 1).pointer.active).toBe(false)
    expect(mapper.snapshot(POINTER_IDLE_MS + 1).pointer.speed).toBe(0)
  })
})

describe('r7 输入统计', () => {
  const counters = () => ({
    date: '',
    todayKeyboard: 0,
    todayMouse: 0,
    totalKeyboard: 0,
    totalMouse: 0,
  })

  it('只有新的按下才 +1', () => {
    const stats = counters()
    const mapper = createPairActivityMapper()

    countStats(stats, mapper.handleKeyboard('KeyA', true, 0), 'keyboard', '2026-09-23')
    countStats(stats, mapper.handleKeyboard('KeyA', true, 10), 'keyboard', '2026-09-23')
    countStats(stats, mapper.handleKeyboard('KeyA', false, 20), 'keyboard', '2026-09-23')
    countStats(stats, mapper.handleKeyboard('KeyB', true, 30), 'keyboard', '2026-09-23')

    expect(stats.todayKeyboard).toBe(2)
    expect(stats.totalKeyboard).toBe(2)

    countStats(stats, mapper.handleMouseButton('Left', true), 'mouse', '2026-09-23')
    countStats(stats, mapper.handleMouseButton('Left', true), 'mouse', '2026-09-23')

    expect(stats.todayMouse).toBe(1)
  })

  it('跨过午夜只归零今日计数', () => {
    const stats = counters()

    stats.date = '2026-09-22'
    stats.todayKeyboard = 100
    stats.totalKeyboard = 1_000

    const rolled = rolloverStats(stats, '2026-09-23')

    expect(rolled).toBe(true)
    expect(stats.todayKeyboard).toBe(0)
    expect(stats.totalKeyboard).toBe(1_000)
    expect(stats.date).toBe('2026-09-23')
    expect(rolloverStats(stats, '2026-09-23')).toBe(false)
  })
})

describe('reset', () => {
  it('清空之后快照回到中性状态', () => {
    const mapper = createPairActivityMapper()

    mapper.handleKeyboard('KeyA', true, 0)
    mapper.handleMouseButton('Right', true)
    mapper.handlePointerRatio(0.1, 0.9, 0)
    mapper.reset()

    const snapshot: PetSnapshot = mapper.snapshot(0)

    expect(snapshot.keyboard).toMatchObject({ active: false, leftHand: false, rightHand: false, keys: [] })
    expect(snapshot.pointer).toMatchObject({ active: false, x: 0.5, y: 0.5, rightDown: false })
  })
})

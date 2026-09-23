/**
 * 把本机输入事件映射成可以联网的宠物快照（§16 - §20）。
 *
 * 这个文件是**纯函数**：不 import Vue / Tauri / Pinia，也不读任何全局状态，
 * 时间全部由调用方传入。这样才能直接单元测试「不泄露按键内容」与 R2 / R3 的口径，
 * 见 docs/pair-plan.md 的 §79 与 R14。
 *
 * 网络层只会看到：哪只手在按、打字强度、鼠标的屏幕比例、按键与速度，
 * 永远看不到具体键名，也永远看不到真实像素坐标。
 */

export interface PetKeyboardState {
  active: boolean
  leftHand: boolean
  rightHand: boolean
  intensity: number
}

export interface PetPointerState {
  active: boolean
  x: number
  y: number
  speed: number
  leftDown: boolean
  rightDown: boolean
}

export interface PetSnapshot {
  keyboard: PetKeyboardState
  pointer: PetPointerState
}

/** `pressed` = 这是一个新的按下（R7 的统计只认它）；`repeat` = 长按或重复事件 */
export type InputOutcome = 'pressed' | 'repeat' | 'released'

/** 强度窗口（R3）：500ms 内去重后的按下集合大小决定强度 */
export const TYPING_WINDOW_MS = 500
/** 鼠标多久没有移动就算不活跃 */
export const POINTER_IDLE_MS = 600
/** 计算鼠标速度的采样窗口 */
export const SPEED_WINDOW_MS = 250

/** 强度档位（R3）：0 / 0.2 / 0.4 / 0.6 / 0.8 / 1.0 */
export const INTENSITY_STEP = 0.2
/** 指针比例量化步长（R4） */
export const POINTER_STEP = 0.02
/** 速度量化步长 */
export const SPEED_STEP = 0.05

/**
 * 鼠标在采样窗口内移动「四分之一个屏幕」算满速。
 * 这个值只影响远端猫的展示细节（§16 的 speed 字段），不影响任何安全口径。
 */
const SPEED_FULL_RATIO = 0.25

/**
 * 修饰键：不参与打字强度（R3），但仍然属于某只手（R2）。
 * CapsLock 与 Fn 也算在这里：它们不是「打字」。
 */
const MODIFIER_KEYS = new Set([
  'ShiftLeft',
  'ShiftRight',
  'ControlLeft',
  'ControlRight',
  'Alt',
  'AltGr',
  'MetaLeft',
  'MetaRight',
  'CapsLock',
  'Function',
])

/**
 * R2 的静态物理分区表。
 *
 * 用 rdev 的原始键名判定，**不走** `getSupportedKey` 的归一化，也不用模型目录里的
 * `left-keys` / `right-keys`（那只是贴图分组，`right-keys` 里只有 4 个方向键）。
 * 没列到的键不进左右手。
 */
const LEFT_HAND_KEYS = new Set([
  // 左侧功能区与修饰键
  'Escape',
  'BackQuote',
  'Tab',
  'CapsLock',
  'ShiftLeft',
  'ControlLeft',
  'Alt',
  'AltGr',
  'MetaLeft',
  'Function',
  'IntlBackslash',
  // 左手数字
  'Num1',
  'Num2',
  'Num3',
  'Num4',
  'Num5',
  // 左手字母
  'KeyQ',
  'KeyW',
  'KeyE',
  'KeyR',
  'KeyT',
  'KeyA',
  'KeyS',
  'KeyD',
  'KeyF',
  'KeyG',
  'KeyZ',
  'KeyX',
  'KeyC',
  'KeyV',
  'KeyB',
  // 空格是双手键，跟着贴图分组（模型里它在 left-keys）算左手
  'Space',
  // 功能键按左侧一半估算
  'F1',
  'F2',
  'F3',
  'F4',
])

const RIGHT_HAND_KEYS = new Set([
  // 右手字母
  'KeyY',
  'KeyU',
  'KeyI',
  'KeyO',
  'KeyP',
  'KeyH',
  'KeyJ',
  'KeyK',
  'KeyL',
  'KeyN',
  'KeyM',
  // 右手数字与符号
  'Num6',
  'Num7',
  'Num8',
  'Num9',
  'Num0',
  'Minus',
  'Equal',
  'Backspace',
  'LeftBracket',
  'RightBracket',
  'BackSlash',
  'SemiColon',
  'Quote',
  'Comma',
  'Dot',
  'Slash',
  'Return',
  // 右侧修饰键
  'ShiftRight',
  'ControlRight',
  'MetaRight',
  // 方向键与中间导航区
  'UpArrow',
  'DownArrow',
  'LeftArrow',
  'RightArrow',
  'Insert',
  'Delete',
  'Home',
  'End',
  'PageUp',
  'PageDown',
  // 小键盘
  'NumLock',
  'KpReturn',
  'KpMinus',
  'KpPlus',
  'KpMultiply',
  'KpDivide',
  'KpDecimal',
  'KpEqual',
  'KpComma',
  'Kp0',
  'Kp1',
  'Kp2',
  'Kp3',
  'Kp4',
  'Kp5',
  'Kp6',
  'Kp7',
  'Kp8',
  'Kp9',
  // 顶部与媒体键
  'PrintScreen',
  'ScrollLock',
  'Pause',
  'VolumeUp',
  'VolumeDown',
  'VolumeMute',
  'Apps',
  'IntlRo',
  'IntlYen',
  'KanaMode',
  'Kana',
  'Hangul',
  'Hanja',
  'Hanji',
  'Lang1',
  'Lang2',
  'Lang3',
  'Lang4',
  'Lang5',
  'F5',
  'F6',
  'F7',
  'F8',
  'F9',
  'F10',
  'F11',
  'F12',
  'F13',
  'F14',
  'F15',
  'F16',
  'F17',
  'F18',
  'F19',
  'F20',
  'F21',
  'F22',
  'F23',
  'F24',
])

/** 裁到 0..1 后量化到 step 的整数倍（与 Rust 侧 `PetSnapshot::sanitized` 同口径） */
export function quantize(value: number, step: number): number {
  if (!Number.isFinite(value)) return 0

  const clamped = Math.min(1, Math.max(0, value))

  // 3 位小数是为了消掉 `Math.round(x / 0.2) * 0.2` 这类二进制浮点的尾巴，
  // 让「量化后是否变化」的比较稳定
  return Math.round((Math.round(clamped / step) * step) * 1000) / 1000
}

/** 与 Rust 的 `sanitized()` 保持一致：对端发来的值也走一遍，UI 只会拿到有界的量化值 */
export function sanitizeSnapshot(snapshot: PetSnapshot): PetSnapshot {
  return {
    keyboard: {
      active: snapshot.keyboard.active,
      leftHand: snapshot.keyboard.leftHand,
      rightHand: snapshot.keyboard.rightHand,
      intensity: quantize(snapshot.keyboard.intensity, INTENSITY_STEP),
    },
    pointer: {
      active: snapshot.pointer.active,
      x: quantize(snapshot.pointer.x, POINTER_STEP),
      y: quantize(snapshot.pointer.y, POINTER_STEP),
      speed: quantize(snapshot.pointer.speed, SPEED_STEP),
      leftDown: snapshot.pointer.leftDown,
      rightDown: snapshot.pointer.rightDown,
    },
  }
}

export function defaultSnapshot(): PetSnapshot {
  return {
    keyboard: {
      active: false,
      leftHand: false,
      rightHand: false,
      intensity: 0,
    },
    pointer: {
      active: false,
      x: 0.5,
      y: 0.5,
      speed: 0,
      leftDown: false,
      rightDown: false,
    },
  }
}

interface PointerSample {
  x: number
  y: number
  at: number
}

export interface PairActivityMapper {
  handleKeyboard: (key: string, pressed: boolean, now: number) => InputOutcome
  handleMouseButton: (button: string, pressed: boolean) => InputOutcome
  handlePointerRatio: (xRatio: number, yRatio: number, now: number) => void
  /** 推进时间窗口（强度衰减、指针不活跃），不会发送任何东西 */
  advance: (now: number) => void
  /** 当前快照：已完成裁剪与量化，可以直接发到网络 */
  snapshot: (now: number) => PetSnapshot
  /** 清空所有状态（peer 离线、暂停同步后恢复时使用） */
  reset: () => void
}

export interface MapperOptions {
  /**
   * 单次按下的最长「按住」时间，0 表示不限制。
   *
   * Windows 下部分系统级按键收不到释放事件（本机也靠 `autoReleaseDelay` 兜底），
   * 少了这一层，远端猫会一直举着一只手。这里只影响左右手显示，不影响统计：
   * 键本身仍然留在按下集合里，所以 OS 自动重复不会被算成新的按下。
   */
  handHoldLimitMs?: () => number
}

export function createPairActivityMapper(options: MapperOptions = {}): PairActivityMapper {
  /** 当前按下的物理键（只用来判断左右手与去重，键名永远不出这个函数） */
  const pressedKeys = new Map<string, number>()
  /** 打字强度窗口内的时间戳 */
  let pressTimes: number[] = []
  const pressedButtons = new Set<string>()
  let pointerSamples: PointerSample[] = []
  let pointerX = 0.5
  let pointerY = 0.5
  /** 用 null 而不是 0 当哨兵：时间戳 0 是合法入参，不能被当成「从没动过」 */
  let pointerMovedAt: number | null = null

  const pruneTypingWindow = (now: number) => {
    const deadline = now - TYPING_WINDOW_MS

    pressTimes = pressTimes.filter(at => at >= deadline)
  }

  const pruneSamples = (now: number) => {
    const deadline = now - SPEED_WINDOW_MS

    pointerSamples = pointerSamples.filter(sample => sample.at >= deadline)
  }

  const currentSpeed = (now: number) => {
    if (pointerSamples.length < 2) return 0

    let travel = 0

    for (let index = 1; index < pointerSamples.length; index++) {
      const previous = pointerSamples[index - 1]
      const current = pointerSamples[index]

      travel += Math.hypot(current.x - previous.x, current.y - previous.y)
    }

    const elapsed = Math.max(now - pointerSamples[0].at, 1)
    const fullSpeedPerMs = SPEED_FULL_RATIO / SPEED_WINDOW_MS

    return (travel / elapsed) / fullSpeedPerMs
  }

  const handleKeyboard = (key: string, pressed: boolean, now: number): InputOutcome => {
    if (pressed) {
      if (pressedKeys.has(key)) return 'repeat'

      pressedKeys.set(key, now)

      if (!MODIFIER_KEYS.has(key)) {
        pressTimes.push(now)
      }

      return 'pressed'
    }

    pressedKeys.delete(key)

    return 'released'
  }

  const handleMouseButton = (button: string, pressed: boolean): InputOutcome => {
    if (pressed) {
      if (pressedButtons.has(button)) return 'repeat'

      pressedButtons.add(button)

      return 'pressed'
    }

    pressedButtons.delete(button)

    return 'released'
  }

  const handlePointerRatio = (xRatio: number, yRatio: number, now: number) => {
    if (!Number.isFinite(xRatio) || !Number.isFinite(yRatio)) return

    pointerX = Math.min(1, Math.max(0, xRatio))
    pointerY = Math.min(1, Math.max(0, yRatio))
    pointerMovedAt = now

    pointerSamples.push({ x: pointerX, y: pointerY, at: now })

    pruneSamples(now)
  }

  const advance = (now: number) => {
    pruneTypingWindow(now)
    pruneSamples(now)
  }

  const reset = () => {
    pressedKeys.clear()
    pressedButtons.clear()
    pressTimes = []
    pointerSamples = []
    pointerMovedAt = null
    pointerX = 0.5
    pointerY = 0.5
  }

  const snapshot = (now: number): PetSnapshot => {
    pruneTypingWindow(now)

    let leftHand = false
    let rightHand = false
    let held = 0
    const holdLimit = options.handHoldLimitMs?.() ?? 0

    for (const [key, pressedAt] of pressedKeys) {
      if (holdLimit > 0 && now - pressedAt > holdLimit) continue

      held += 1

      if (LEFT_HAND_KEYS.has(key)) leftHand = true
      if (RIGHT_HAND_KEYS.has(key)) rightHand = true
    }

    // §18 / R3：500ms 内去重后的按下数，5 次以上封顶
    const intensity = Math.min(pressTimes.length, 5) / 5
    const pointerActive = pointerMovedAt !== null && now - pointerMovedAt <= POINTER_IDLE_MS

    return sanitizeSnapshot({
      keyboard: {
        active: held > 0 || intensity > 0,
        leftHand,
        rightHand,
        intensity,
      },
      pointer: {
        active: pointerActive,
        x: pointerX,
        y: pointerY,
        speed: pointerActive ? currentSpeed(now) : 0,
        leftDown: pressedButtons.has('Left'),
        rightDown: pressedButtons.has('Right'),
      },
    })
  }

  return {
    handleKeyboard,
    handleMouseButton,
    handlePointerRatio,
    advance,
    snapshot,
    reset,
  }
}

/** 计数载体：结构上就等于 pair store 里的 `PairStats`，避免让纯函数 import 任何 store */
export interface StatCounters {
  date: string
  todayKeyboard: number
  todayMouse: number
  totalKeyboard: number
  totalMouse: number
}

/**
 * 跨过本地午夜时把今日计数归零。返回 true 表示刚发生了归零。
 */
export function rolloverStats(stats: StatCounters, date: string): boolean {
  if (stats.date === date) return false

  stats.date = date
  stats.todayKeyboard = 0
  stats.todayMouse = 0

  return true
}

/**
 * 只统计次数、不记录内容（§24 / R7）。
 *
 * 只有映射器判定为「新的按下」时才 +1，因此长按产生的 OS 重复事件与 Windows 的
 * 3 秒自动释放都不会重复计数。
 */
export function countStats(
  stats: StatCounters,
  outcome: InputOutcome,
  source: 'keyboard' | 'mouse',
  date: string,
) {
  const rolledOver = rolloverStats(stats, date)

  if (outcome !== 'pressed') {
    return rolledOver
  }

  if (source === 'keyboard') {
    stats.todayKeyboard += 1
    stats.totalKeyboard += 1
  } else {
    stats.todayMouse += 1
    stats.totalMouse += 1
  }

  return rolledOver
}

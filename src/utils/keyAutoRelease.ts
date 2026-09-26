/**
 * Windows 上「按键自动释放」的计时器（纯逻辑，见 `keyAutoRelease.spec.ts`）。
 *
 * 为什么要有它：Windows 下部分系统级按键收不到释放事件（设置项「按键自动释放延迟」就是
 * 给它们准备的），不能因为一直没收到抬起就让贴图一直亮着。
 *
 * 但它**不能**把「安静」当成「抬起」：Windows 的键盘自动重复只跟**最后按下**的那个键，
 * 按住 w 再按 a/d 之后 w 会彻底安静（松开 a/d 也不会恢复重复）。于是延迟一到 w 就被当成
 * 已经松开——用户报的「按住 w、中间按过 ad，最后 w 明明按着却不亮」就是这么来的
 * （`keyHighlight.ts` 的 heldKeys 保证「真的按着就还能回退」，前提是键没被这里删掉）。
 *
 * 所以到点之后先问一次系统（`isKeyStillDown`）：还按着就续一轮，真的抬起了才释放。
 */

export interface KeyAutoReleasePressOptions {
  /** 这一轮第一次等多久（毫秒）。不给就用 `delay()`；续轮一律用当时的 `delay()` */
  delay?: number
  /**
   * 到点要不要问系统。默认要；`false` 用于 CapsLock 这种「按一下亮一下就灭」的显示
   * （它的按下状态是切换语义，问系统没有意义）。
   */
  probe?: boolean
}

export interface KeyAutoReleaseOptions {
  /** 一次等多久（毫秒）。每次按下都会重排 */
  delay: () => number
  /** 这些 rdev 原始键名里有没有还按着的；拿不到答案时返回 false（= 按老规矩释放） */
  isKeyStillDown: (rawKeys: string[]) => Promise<boolean>
  /** 到点、且确认已经抬起：把显示层松开 */
  onRelease: (key: string) => void
  /** 到点但还按着（续一轮）时调用：联机同步要用它把「其实还按着」续期 */
  onStillDown?: (rawKeys: string[]) => void
}

export interface KeyAutoRelease {
  /**
   * 记下一次按下。`key` 是显示用的键名（贴图名，如 `KeyW` / `Fn` / `Shift`），
   * `raw` 是 rdev 的原始键名（如 `KeyW` / `F5` / `ShiftRight`）——多个原始键可能
   * 归一化成同一个显示名，问系统时它们要一起问。
   */
  press: (key: string, raw: string, pressOptions?: KeyAutoReleasePressOptions) => void
  /** 真的收到了抬起事件：撤掉待发的这一轮，也忘掉它的原始键名 */
  release: (key: string) => void
  /** 撤掉所有计时器（窗口卸载时用） */
  stop: () => void
}

/**
 * 续轮的最短等待：设置里能把延迟调得很小（0.5 秒），而每一轮都要问一次系统（一次 IPC）。
 * 给续轮加个下限，免得把节奏调得太密。
 */
const RE_ARM_MIN_MS = 1000

export function createKeyAutoRelease(options: KeyAutoReleaseOptions): KeyAutoRelease {
  const timers = new Map<string, ReturnType<typeof setTimeout>>()
  /** 显示名 → 它由哪些原始键名归一化来的（`Fn` ← F1..F12，`Shift` ← 左右 Shift） */
  const rawNames = new Map<string, Set<string>>()
  /**
   * 显示名 → 第几轮。`press` / `release` 都会开新的一轮。
   *
   * 到点后的「问系统」是异步的，这一问的工夫里这个键可能又按下/抬起了（OS 自动重复、
   * 快速点按），那一轮该由新的事件负责：这一轮比对轮次就自己退出去，不会插一脚。
   */
  const generations = new Map<string, number>()

  const arm = (key: string, delay: number, probe: boolean) => {
    const pending = timers.get(key)

    if (pending !== undefined) clearTimeout(pending)

    const generation = (generations.get(key) ?? 0) + 1

    generations.set(key, generation)

    timers.set(key, setTimeout(async () => {
      if (generations.get(key) !== generation) return

      timers.delete(key)

      const candidates = [...rawNames.get(key) ?? []]

      if (probe && candidates.length > 0 && await options.isKeyStillDown(candidates)) {
        if (generations.get(key) !== generation) return

        options.onStillDown?.(candidates)

        arm(key, Math.max(options.delay(), RE_ARM_MIN_MS), probe)

        return
      }

      if (generations.get(key) !== generation) return

      options.onRelease(key)
    }, delay))
  }

  return {
    press(key, raw, pressOptions) {
      const names = rawNames.get(key) ?? new Set<string>()

      names.add(raw)
      rawNames.set(key, names)

      arm(key, pressOptions?.delay ?? options.delay(), pressOptions?.probe ?? true)
    },
    release(key) {
      const pending = timers.get(key)

      if (pending !== undefined) clearTimeout(pending)

      timers.delete(key)
      rawNames.delete(key)
      generations.set(key, (generations.get(key) ?? 0) + 1)
    },
    stop() {
      for (const pending of timers.values()) clearTimeout(pending)

      timers.clear()
      rawNames.clear()
      generations.clear()
    },
  }
}

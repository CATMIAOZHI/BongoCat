import dayjs from 'dayjs'
import { defineStore } from 'pinia'
import { reactive, ref } from 'vue'

import type { PairStats } from './pair'

/**
 * 输入统计单独一个 store。
 *
 * 为什么必须和 pair store 分开：`@tauri-store/pinia` 同步时发给后端的永远是**整份状态**，
 * 而猫咪窗口每敲一次键、每点一下鼠标都要改这里的数字。统计和设置挤在同一个 store 里时，
 * 这些高频 patch 每次都会夹带一份别人那份（旧的）`settings`，把偏好页正在改的设置盖回去——
 * 「拖滑块弹回去 / 开关自己跳回去 / 暂离文字打字时丢字」都是这一个原因。分开之后，
 * 高频 patch 只带统计，设置只在真的被改时由偏好页发出去。
 */
export const usePairStatsStore = defineStore('pair-stats', () => {
  const stats = reactive<PairStats>({
    date: '',
    todayKeyboard: 0,
    todayMouse: 0,
    totalKeyboard: 0,
    totalMouse: 0,
  })

  /** 是否已经把老 pair store 里的那份统计搬过来了（拆分前它们挤在同一个 store 里） */
  const adoptedLegacy = ref(false)

  return {
    stats,
    adoptedLegacy,
  }
}, {
  tauri: {
    /**
     * 不自动启动：迁移的写入必须发生在**这个 store 载入完**（`watch` 挂上）之后，
     * 否则写进去的数字不会被推给后端，也就存不下来。载入完成的时刻只有 `await start()`
     * 拿得到，所以由 `App.vue`（猫咪窗口里是 `usePairState`）显式启动，见 `markPairStatsLoaded`。
     */
    autoStart: false,
  },
})

/**
 * 要不要把老 pair store 里的统计搬过来。
 *
 * 老的**任一项**比新的多就搬：拆分之后新 store 只会越攒越多，一旦两项都超过老数字就不用再搬了
 * （老数字被冻结在拆分那一刻，永远不会反过来更大）；`adopted` 挡住重复搬。用户的旧数字
 * 同时也留在 `pair.json` 里没动过，算是一份备份。
 *
 * 这里比大小而不是要求「新的必须是 0」：启动瞬间用户可能正好在敲键，新 store 里先有了 1，
 * 用「必须是 0」就会永远搬不过来——那才是真的丢数据。
 */
export function shouldAdoptLegacyStats(options: {
  adopted: boolean
  current: PairStats
  legacy?: Partial<PairStats>
}): boolean {
  const { adopted, current, legacy } = options

  if (adopted) return false
  if (!legacy) return false

  return (legacy.totalKeyboard ?? 0) > current.totalKeyboard
    || (legacy.totalMouse ?? 0) > current.totalMouse
}

/** 老 pair store 里那份统计；等 `pair-stats` 载入完再搬 */
let pendingLegacyStats: Partial<PairStats> | undefined
/** `pair-stats` 是否已经载入完（可以安全写入了） */
let statsStoreLoaded = false

/**
 * 从 pair store 的状态里接过老那份输入统计，并把 `stats` 摘掉。
 *
 * 由 pair store 的 `beforeFrontendSync` 钩子调用。那个钩子每次都拿到「文件里 / 别的窗口发来的
 * 那份完整 pair 状态」，而老用户的累计数字只躺在那里（拆分前统计和设置挤在同一个 store）。
 * 摘掉之后 `stats` 不再进入 pair store 的状态，也就不会跟着每次设置变更广播给别的窗口。
 */
export function takeLegacyStats(state: Record<string, unknown>): Record<string, unknown> {
  const { stats, ...rest } = state

  if (stats) {
    pendingLegacyStats = stats as Partial<PairStats>
    adoptPendingStats()
  }

  return rest
}

/**
 * `pair-stats` 已经载入完，可以把它该有的数字写进去了。
 *
 * 两个调用点（`App.vue` 与猫咪窗口的 `usePairState`）谁先谁后都行：这份统计先到就先压着，
 * 载入完成时再搬；反过来载入先完成也没关系，统计一到手就搬。
 */
export function markPairStatsLoaded() {
  statsStoreLoaded = true
  adoptPendingStats()
}

function adoptPendingStats() {
  const legacy = pendingLegacyStats

  if (!statsStoreLoaded || !legacy) return

  // 只试一次：判据不满足（比如新的已经有数字了）就不用再留着它了
  pendingLegacyStats = void 0

  const statsStore = usePairStatsStore()

  if (!shouldAdoptLegacyStats({
    adopted: statsStore.adoptedLegacy,
    current: statsStore.stats,
    legacy,
  })) {
    return
  }

  // 累计逐项取大，谁都不会被算少
  statsStore.stats.totalKeyboard = Math.max(statsStore.stats.totalKeyboard, legacy.totalKeyboard ?? 0)
  statsStore.stats.totalMouse = Math.max(statsStore.stats.totalMouse, legacy.totalMouse ?? 0)

  /**
   * 「今日」只在老那份也是今天时才搬。
   *
   * 老数字冻在拆分那一刻：那天要是已经过去，这几个数早就不算「今天」了，搬过来只会让
   * 偏好页拿昨天的数字当今天显示，直到下一次 `rolloverStats`（最多 30 秒或下一次按键）
   * 才归零。不是同一天就留给 `rolloverStats` 处理——它的语义就是「换一天重新数」。
   */
  if (legacy.date === dayjs().format('YYYY-MM-DD')) {
    statsStore.stats.date = legacy.date
    statsStore.stats.todayKeyboard = Math.max(statsStore.stats.todayKeyboard, legacy.todayKeyboard ?? 0)
    statsStore.stats.todayMouse = Math.max(statsStore.stats.todayMouse, legacy.todayMouse ?? 0)
  }

  statsStore.adoptedLegacy = true
}

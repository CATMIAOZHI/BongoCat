import dayjs from 'dayjs'
import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it } from 'vitest'

import type { PairStats } from './pair'

import { markPairStatsLoaded, shouldAdoptLegacyStats, takeLegacyStats, usePairStatsStore } from './pairStats'

function stats(partial: Partial<PairStats> = {}): PairStats {
  return {
    date: '',
    todayKeyboard: 0,
    todayMouse: 0,
    totalKeyboard: 0,
    totalMouse: 0,
    ...partial,
  }
}

/**
 * 输入统计从 pair store 拆出来之后的一次性迁移：老用户的累计数字在旧的那份文件里，
 * 搬错了就是丢数据，所以判据钉在这里。
 */
describe('输入统计的迁移判据', () => {
  it('老的还没搬过、新的是空的：搬', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: false,
        current: stats(),
        legacy: stats({ date: '2026-09-25', totalKeyboard: 1_892, totalMouse: 1_418 }),
      }),
    ).toBe(true)
  })

  it('已经搬过就不再搬（防止每次启动都覆盖新数据）', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: true,
        current: stats(),
        legacy: stats({ totalKeyboard: 1_892 }),
      }),
    ).toBe(false)
  })

  it('新的这份已经攒得比老的多：不动它', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: false,
        current: stats({ totalKeyboard: 2_000, totalMouse: 1_500 }),
        legacy: stats({ totalKeyboard: 1_892 }),
      }),
    ).toBe(false)
  })

  it('新的只有一点点（启动瞬间正好敲了一下键）：还是要搬', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: false,
        current: stats({ totalKeyboard: 1 }),
        legacy: stats({ totalKeyboard: 1_892, totalMouse: 1_418 }),
      }),
    ).toBe(true)
  })

  it('逐项比：新的鼠标已经比老的多、但键盘还没赶上，仍然要搬（搬的时候取大的那一份）', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: false,
        current: stats({ totalMouse: 2_000 }),
        legacy: stats({ totalKeyboard: 1_892, totalMouse: 1_418 }),
      }),
    ).toBe(true)
  })

  it('老的那份本来就没有数字（新装的用户）：不搬', () => {
    expect(
      shouldAdoptLegacyStats({
        adopted: false,
        current: stats(),
        legacy: stats({ date: '2026-09-25' }),
      }),
    ).toBe(false)
  })
})

/**
 * 判据之外的那半条链路：谁在什么时候把数字写进新 store。
 *
 * `pair-stats` 不是 `autoStart` 的（载入完 `watch` 才挂上，之前写进去的白写），所以要核
 * 「统计先到」与「载入先完成」两种顺序。`statsStoreLoaded` 是模块级的，本文件里前一个用例
 * 置过真之后就不会再回假，所以那个用例必须排在最前面。
 */
describe('把老统计搬进新 store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  const legacy = (partial: Partial<PairStats> = {}): PairStats => stats(partial)

  it('统计先到就先压着，等 store 载入完再搬', () => {
    const store = usePairStatsStore()

    takeLegacyStats({ settings: {}, stats: legacy({ totalKeyboard: 1_892, totalMouse: 1_418 }) })

    // 还没载入完：这时候写进去不会被推给后端，等于白写
    expect(store.stats.totalKeyboard).toBe(0)

    markPairStatsLoaded()

    expect(store.stats.totalKeyboard).toBe(1_892)
    expect(store.stats.totalMouse).toBe(1_418)
    expect(store.adoptedLegacy).toBe(true)
  })

  it('store 已经载入完，统计一到就搬', () => {
    markPairStatsLoaded()

    const store = usePairStatsStore()

    takeLegacyStats({ stats: legacy({ totalKeyboard: 10 }) })

    expect(store.stats.totalKeyboard).toBe(10)
  })

  it('老那份是前几天的：只搬累计，今日留给 rolloverStats 归零', () => {
    markPairStatsLoaded()

    const store = usePairStatsStore()

    takeLegacyStats({
      stats: legacy({ date: '2020-01-01', todayKeyboard: 12, todayMouse: 3, totalKeyboard: 1_892 }),
    })

    expect(store.stats.totalKeyboard).toBe(1_892)
    expect(store.stats.todayKeyboard).toBe(0)
    expect(store.stats.date).toBe('')
  })

  it('老那份就是今天的：今日也搬，但两边都是「取大的那一份」', () => {
    markPairStatsLoaded()

    const store = usePairStatsStore()
    const today = dayjs().format('YYYY-MM-DD')

    // 新那份的鼠标（累计与今日）已经比老的多：搬的时候不能被改小
    store.stats.totalMouse = 2_000
    store.stats.todayMouse = 99
    store.stats.todayKeyboard = 5

    takeLegacyStats({ stats: legacy({ date: today, todayKeyboard: 12, totalKeyboard: 1_892 }) })

    expect(store.stats.totalKeyboard).toBe(1_892)
    expect(store.stats.totalMouse).toBe(2_000)
    expect(store.stats.todayKeyboard).toBe(12)
    expect(store.stats.todayMouse).toBe(99)
    expect(store.stats.date).toBe(today)
  })

  it('已经搬过就不再搬', () => {
    markPairStatsLoaded()

    const store = usePairStatsStore()

    store.adoptedLegacy = true

    takeLegacyStats({ stats: legacy({ totalKeyboard: 1_892 }) })

    expect(store.stats.totalKeyboard).toBe(0)
  })

  it('文件被手改脏了（stats 不是对象）：不搬、也不炸', () => {
    markPairStatsLoaded()

    const store = usePairStatsStore()

    takeLegacyStats({ stats: null })
    takeLegacyStats({ stats: 'oops' })

    expect(store.stats.totalKeyboard).toBe(0)
    expect(store.adoptedLegacy).toBe(false)

    // 脏值只是被丢掉，别把后面的迁移也一起毒死
    takeLegacyStats({ stats: legacy({ totalKeyboard: 7, totalMouse: 8 }) })

    expect(store.stats.totalKeyboard).toBe(7)
    expect(store.stats.totalMouse).toBe(8)
    expect(store.adoptedLegacy).toBe(true)
  })
})

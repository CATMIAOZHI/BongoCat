import dayjs from 'dayjs'
import { isEqual } from 'es-toolkit'
import { onMounted, onUnmounted, watch } from 'vue'

import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import { isWindows } from '@/utils/platform'

import type { PresenceState } from './usePair'
import type { PetSnapshot } from './usePairActivity'

import { getSupportedKey } from './useModel'
import { pairConnect, pairDisconnect, pairSendPetState, pairSendPresence, pairSendStats } from './usePair'
import {
  countStats,
  createPairActivityMapper,
  rolloverStats,
  sanitizeSnapshot,
} from './usePairActivity'
import { usePairStatus } from './usePairStatus'

/**
 * 拿不到 Rust 给的上限时的兜底：v1 的 3Hz（R4）。
 *
 * 真正使用的间隔由 `store.runtime.petStateHz` 决定（§6 / R23）——P2P 与额度够的自建
 * 中继是 60Hz，CF 缺省仍是 3Hz。这里只负责兜住「状态还没到」的那一小段。
 */
const FALLBACK_SNAPSHOT_INTERVAL_MS = 333
/** §25：统计最多每 30 秒一次 */
const STATS_INTERVAL_MS = 30_000
/** §27：鼠标累计移动超过 4px 才算「真的回来了」 */
const AWAY_MOVE_THRESHOLD_PX = 4

export interface PointerPoint {
  x: number
  y: number
}

/**
 * 本地一侧的联机状态：把输入事件变成宠物快照、维护统计与暂离状态。
 *
 * 只在猫咪窗口（唯一挂载 `useDevice` 的页面）里创建一次：它需要本机的输入事件，
 * 也需要「对端是否在线」这个唯一来自网络的状态（通过 `usePairStatus` 订阅）。
 */
export function usePairState() {
  const store = usePairStore()
  const catStore = useCatStore()
  const modelStore = useModelStore()

  // peer 是否在线决定了该不该发送，所以这里必须先订阅 Rust 的连接状态
  usePairStatus()

  const mapper = createPairActivityMapper({
    handHoldLimitMs: () => {
      // Windows 下有些系统级按键收不到释放事件，用本机同样的自动释放延迟兜底
      if (!isWindows) return 0

      return Math.max(catStore.model.autoReleaseDelay, 1) * 1000
    },
    // R37：只有本机模型真的有这张贴图的键才发出去（对端还要用它自己的模型再认一遍）
    isSupportedKey: (key) => {
      return Boolean(modelStore.supportKeys[getSupportedKey(modelStore.supportKeys, key)])
    },
  })

  let lastSnapshot: PetSnapshot | undefined
  let lastSentAt = 0
  let trailingTimer: ReturnType<typeof setTimeout> | undefined
  let statsTimer: ReturnType<typeof setInterval> | undefined
  let lastPointer: PointerPoint | undefined
  let awayDistance = 0

  const today = () => dayjs().format('YYYY-MM-DD')

  const canSendActivity = () => {
    return store.settings.enabled
      && store.runtime.peerOnline
      && !store.settings.privacy.pauseActivitySync
  }

  const canSendStats = () => {
    return canSendActivity() && store.settings.privacy.shareInputStats
  }

  /** 当前生效传输允许的快照间隔（毫秒）。上限只由 Rust 给，这里只做兜底。 */
  const snapshotIntervalMs = () => {
    const hz = store.runtime.petStateHz

    return hz > 0 ? 1000 / hz : FALLBACK_SNAPSHOT_INTERVAL_MS
  }

  /** 隐私开关为关闭时，对应部分一律发「空值」，而不是发出去再让对端忽略 */
  const applyPrivacy = (snapshot: PetSnapshot): PetSnapshot => {
    const { shareTypingActivity, sharePointer } = store.settings.privacy

    if (shareTypingActivity && sharePointer) return snapshot

    return sanitizeSnapshot({
      keyboard: shareTypingActivity
        ? snapshot.keyboard
        : { active: false, leftHand: false, rightHand: false, intensity: 0, keys: [] },
      pointer: sharePointer
        ? snapshot.pointer
        : { active: false, x: 0.5, y: 0.5, speed: 0, leftDown: false, rightDown: false },
    })
  }

  const sendSnapshot = (force = false) => {
    if (!canSendActivity()) return

    const now = Date.now()

    mapper.advance(now)

    const snapshot = applyPrivacy(mapper.snapshot(now))

    // R4：量化后没有任何变化就不发
    if (!force && lastSnapshot && isEqual(snapshot, lastSnapshot)) return

    const elapsed = now - lastSentAt

    const interval = snapshotIntervalMs()

    if (!force && elapsed < interval) {
      // 变化发生了但还没到刷新间隔：补一次尾随发送，保证最终状态一定会送达
      if (trailingTimer) clearTimeout(trailingTimer)

      trailingTimer = setTimeout(() => {
        trailingTimer = void 0

        sendSnapshot()
      }, interval - elapsed)

      return
    }

    lastSnapshot = snapshot
    lastSentAt = now

    void pairSendPetState(snapshot).catch(() => void 0)
  }

  const sendStats = () => {
    if (!canSendActivity()) return

    const share = store.settings.privacy.shareInputStats

    void pairSendStats({
      date: today(),
      // 不分享时连数字都不发出去，只告诉对端「我不分享」
      todayKeyboard: share ? store.stats.todayKeyboard : 0,
      todayMouse: share ? store.stats.todayMouse : 0,
      totalKeyboard: share ? store.stats.totalKeyboard : 0,
      totalMouse: share ? store.stats.totalMouse : 0,
      share,
    }).catch(() => void 0)
  }

  const sendPresence = (presence: PresenceState) => {
    if (!store.runtime.peerOnline) return

    const message = presence === 'away' ? store.settings.away.message.trim() : ''

    void pairSendPresence(
      presence,
      message || void 0,
      store.settings.identity.displayName.trim() || void 0,
    ).catch(() => void 0)
  }

  const setPresence = (presence: PresenceState) => {
    awayDistance = 0
    lastPointer = void 0
    store.settings.presence = presence
  }

  /** §27：键盘、点击或明显的鼠标移动都应该把人从「暂离」里带回来 */
  const wake = () => {
    if (store.settings.presence !== 'away') return
    if (!store.settings.away.autoReturn) return

    setPresence('active')
  }

  const count = (outcome: 'pressed' | 'repeat' | 'released', source: 'keyboard' | 'mouse') => {
    // 跨过本地午夜时先把今日计数归零（countStats 的返回值就是这件事发生了）
    if (countStats(store.stats, outcome, source, today())) {
      sendStats()
    }
  }

  const handleKeyboard = (key: string, pressed: boolean) => {
    const outcome = mapper.handleKeyboard(key, pressed, Date.now())

    if (outcome === 'pressed') {
      count(outcome, 'keyboard')
      wake()
    }

    sendSnapshot()
  }

  const handleMouseButton = (button: string, pressed: boolean) => {
    const outcome = mapper.handleMouseButton(button, pressed)

    if (outcome === 'pressed') {
      count(outcome, 'mouse')
      wake()
    }

    sendSnapshot()
  }

  /** 指针比例：由 useDevice 按 R12 只算一次后传进来 */
  const handlePointerRatio = (xRatio: number, yRatio: number) => {
    mapper.handlePointerRatio(xRatio, yRatio, Date.now())
    sendSnapshot()
  }

  /** 原始物理坐标只用于 §27 的「移动超过 4px 自动回来」 */
  const handlePointerMove = (point: PointerPoint) => {
    if (store.settings.presence === 'away') {
      if (lastPointer) {
        awayDistance += Math.hypot(point.x - lastPointer.x, point.y - lastPointer.y)
      }

      if (awayDistance > AWAY_MOVE_THRESHOLD_PX) {
        wake()
      }
    }

    lastPointer = point
  }

  const resetActivity = () => {
    mapper.reset()
    lastSnapshot = void 0
    lastSentAt = 0
    lastPointer = void 0
    awayDistance = 0

    if (trailingTimer) {
      clearTimeout(trailingTimer)

      trailingTimer = void 0
    }
  }

  onMounted(() => {
    // 启动时先按本地日期校正一次，避免上次运行的「今日」数据被当成今天
    if (rolloverStats(store.stats, today())) sendStats()

    statsTimer = setInterval(() => {
      if (rolloverStats(store.stats, today())) sendStats()

      if (canSendStats()) sendStats()
    }, STATS_INTERVAL_MS)

    // §74：开机启动后按设置自动连接
    if (store.settings.enabled && store.settings.relay.autoConnect && store.settings.relay.url) {
      void pairConnect(store.settings.relay.url).catch(() => void 0)
    }
  })

  onUnmounted(() => {
    if (statsTimer) clearInterval(statsTimer)
    if (trailingTimer) clearTimeout(trailingTimer)
  })

  watch(() => store.settings.presence, (presence) => {
    sendPresence(presence)
  })

  watch(() => store.runtime.peerOnline, (online) => {
    resetActivity()

    if (!online) return

    // §20 / §25：刚连上先同步一次 presence 与统计，再发一帧当前活动
    sendPresence(store.settings.presence)
    sendStats()
    sendSnapshot(true)
  })

  watch(() => store.settings.privacy.shareInputStats, () => {
    // 开与关都要立刻同步一次：关闭时发的是 share=false 的清零载荷，
    // 对端据此清掉旧数字，而不是继续显示上一次的统计
    sendStats()
  })

  watch(() => store.settings.privacy.pauseActivitySync, (paused) => {
    if (paused) {
      // §52：暂停后不再发送活动，先让对方把猫放下
      void pairSendPetState(
        sanitizeSnapshot({
          keyboard: { active: false, leftHand: false, rightHand: false, intensity: 0, keys: [] },
          pointer: { active: false, x: 0.5, y: 0.5, speed: 0, leftDown: false, rightDown: false },
        }),
      ).catch(() => void 0)
    } else {
      resetActivity()
      sendSnapshot(true)
    }
  })

  watch(() => store.settings.enabled, (enabled) => {
    if (!enabled) {
      store.runtime.connection = 'disabled'

      void pairDisconnect().catch(() => void 0)

      return
    }

    if (store.settings.relay.autoConnect && store.settings.relay.url) {
      void pairConnect(store.settings.relay.url).catch(() => void 0)
    }
  })

  return {
    /** 只给 useDevice 用：远程同步需要的输入 */
    handleKeyboard,
    handleMouseButton,
    handlePointerRatio,
    handlePointerMove,
  }
}

import dayjs from 'dayjs'
import { isEqual } from 'es-toolkit'
import { onMounted, onUnmounted, watch } from 'vue'

import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import { markPairStatsLoaded, usePairStatsStore } from '@/stores/pairStats'
import { shouldSendKeepAlive } from '@/utils/keepAlive'
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
/**
 * 「还按着东西」时的重发节奏（R46）。
 *
 * 对方猫的按键/爪子 TTL 是 800ms、点击是 500ms，看的是**收到包的时间**；而按住键不动时
 * 快照没有任何变化（R4：量化后一样就不发），对面就会自己把贴图与爪子放下来。所以还按着
 * 东西的时候要把同一份快照再发一次：间隔取 `max(这个值, 当前快照间隔)`——250ms 压得住两个
 * TTL，又远低于传输额度（自建中继 20 帧/秒、DC 那条腿 60Hz）。判据本身在
 * `utils/keepAlive.ts`（含「窗口藏起来就不补」）。
 *
 * `KEEP_ALIVE_TICK_MS` 只是检查节奏：它要明显比上面那个下限小，否则 CF 那种 333ms 的快照
 * 间隔会被「250 + 250」凑成 500ms，正好压不住点击的 500ms TTL。
 */
const KEEP_ALIVE_MIN_MS = 250
const KEEP_ALIVE_TICK_MS = 50
/** §27：鼠标累计移动超过 4px 才算「真的回来了」 */
const AWAY_MOVE_THRESHOLD_PX = 4
/**
 * 进入暂离后，要先安静这么久（没按键、没点击、没明显移动鼠标）才开始检测「回来了」。
 *
 * 「我暂离」多半是用鼠标点的（设置页开关、托盘菜单；另有快捷键），点完一挪鼠标就超过 4px，
 * 不等这一下的话暂离会立刻被自动结束，看起来就是「打不开」。
 */
const AWAY_ARM_IDLE_MS = 5000

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
  const statsStore = usePairStatsStore()
  const catStore = useCatStore()
  const modelStore = useModelStore()

  // peer 是否在线决定了该不该发送，所以这里必须先订阅 Rust 的连接状态
  usePairStatus()

  const mapper = createPairActivityMapper({
    handHoldLimitMs: () => {
      // Windows 下有些系统级按键收不到释放事件，用本机同样的自动释放延迟兜底
      if (!isWindows) return 0

      // 比本机多留 1 秒：本机到点要先问一次系统才续期（一次 IPC），这段时间里这一帧会被
      // 按「超过按住上限」裁掉发出去，对方猫就缺一帧；余量比探询往返大得多就撞不上
      return Math.max(catStore.model.autoReleaseDelay, 1) * 1000 + 1000
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
  let keepAliveTimer: ReturnType<typeof setInterval> | undefined
  let lastPointer: PointerPoint | undefined
  let awayDistance = 0
  /** 暂离期间最近一次有输入的时间；距今超过 AWAY_ARM_IDLE_MS 后，下一次输入才算回来 */
  let awayLastInputAt = 0

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
      todayKeyboard: share ? statsStore.stats.todayKeyboard : 0,
      todayMouse: share ? statsStore.stats.todayMouse : 0,
      totalKeyboard: share ? statsStore.stats.totalKeyboard : 0,
      totalMouse: share ? statsStore.stats.totalMouse : 0,
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

  /**
   * 还按着东西时把同一份快照再发一次：对方猫的 TTL 才不至于把还按着的键/爪子放下来。
   *
   * 重算一遍再发：这段时间里发送侧自己的状态可能变了（比如某个键到了按住上限），
   * 不能把一份陈旧的「还按着」一直发下去。
   */
  const sendKeepAlive = () => {
    if (!canSendActivity() || !lastSnapshot) return

    const { keyboard, pointer } = lastSnapshot
    const now = Date.now()

    if (!shouldSendKeepAlive({
      keys: keyboard.keys,
      leftDown: pointer.leftDown,
      rightDown: pointer.rightDown,
      sinceLastSentMs: now - lastSentAt,
      minIntervalMs: Math.max(KEEP_ALIVE_MIN_MS, snapshotIntervalMs()),
      pageVisible: document.visibilityState === 'visible',
    })) {
      return
    }

    mapper.advance(now)

    const snapshot = applyPrivacy(mapper.snapshot(now))

    lastSnapshot = snapshot
    lastSentAt = now

    void pairSendPetState(snapshot).catch(() => void 0)
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

    const now = Date.now()

    // 刚进入暂离、人还在电脑前操作：只刷新时间，等真正离开后再检测
    if (now - awayLastInputAt < AWAY_ARM_IDLE_MS) {
      awayLastInputAt = now
      awayDistance = 0

      return
    }

    setPresence('active')
  }

  const count = (outcome: 'pressed' | 'repeat' | 'released', source: 'keyboard' | 'mouse') => {
    // 跨过本地午夜时先把今日计数归零（countStats 的返回值就是这件事发生了）
    if (countStats(statsStore.stats, outcome, source, today())) {
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

  /**
   * 系统确认这些键还按着（R46）：给发送侧的按键续期，别让对方猫看到它掉下去。
   *
   * 续期可能把刚被按住上限剔掉的键找回来，所以这里再算一次快照。
   */
  const noteKeysStillDown = (keys: readonly string[]) => {
    mapper.noteKeysStillDown(keys, Date.now())
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

  /** 让对方立刻把猫放下：暂停同步、以及 R39 进输入框时都用这一份空快照 */
  const clearedSnapshot = () => {
    return sanitizeSnapshot({
      keyboard: { active: false, leftHand: false, rightHand: false, intensity: 0, keys: [] },
      pointer: { active: false, x: 0.5, y: 0.5, speed: 0, leftDown: false, rightDown: false },
    })
  }

  onMounted(async () => {
    // 输入统计单独一个 store，且不由 `autoStart` 启动：迁移老数字的写入必须发生在它载入完
    // （`watch` 挂上）之后，否则写进去的数字不会被推给后端。这里 await 到那一刻再往下走。
    await statsStore.$tauri.start()
    markPairStatsLoaded()

    // 启动时先按本地日期校正一次，避免上次运行的「今日」数据被当成今天
    if (rolloverStats(statsStore.stats, today())) sendStats()

    statsTimer = setInterval(() => {
      if (rolloverStats(statsStore.stats, today())) sendStats()

      if (canSendStats()) sendStats()
    }, STATS_INTERVAL_MS)

    keepAliveTimer = setInterval(sendKeepAlive, KEEP_ALIVE_TICK_MS)

    // §74：开机启动后按设置自动连接
    if (store.settings.enabled && store.settings.relay.autoConnect && store.settings.relay.url) {
      void pairConnect(store.settings.relay.url).catch(() => void 0)
    }
  })

  onUnmounted(() => {
    if (statsTimer) clearInterval(statsTimer)
    if (keepAliveTimer) clearInterval(keepAliveTimer)
    if (trailingTimer) clearTimeout(trailingTimer)
  })

  watch(() => store.settings.presence, (presence) => {
    if (presence === 'away') {
      // 暂离可能是别的窗口（设置页 / 聊天 / 托盘）改的，这里统一从头计时
      awayLastInputAt = Date.now()
      awayDistance = 0
      lastPointer = void 0
    }

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

  /**
   * 暂离期间改了举牌文字：立刻按新的文字重发一次 presence。
   *
   * 偏好页那边是「草稿 + 保存」，保存只改 store 里的值；不重发的话对方猫头上的牌子
   * 会一直停在旧文字上，直到下次切暂离才更新——按钮看着生效了、对面却没变。
   */
  watch(() => store.settings.away.message, () => {
    if (store.settings.presence === 'away') sendPresence('away')
  })

  watch(() => store.settings.privacy.pauseActivitySync, (paused) => {
    if (paused) {
      // §52：暂停后不再发送活动，先让对方把猫放下
      void pairSendPetState(clearedSnapshot()).catch(() => void 0)
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
    noteKeysStillDown,
  }
}

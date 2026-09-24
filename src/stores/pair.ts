import { defineStore } from 'pinia'
import { reactive, ref } from 'vue'

import type { P2pState, PresenceState } from '@/composables/usePair'

import { ATTACHMENT_MAX_MB } from '@/composables/usePair'

export type PairConnectionState
  = | 'disabled'
    | 'disconnected'
    | 'connecting'
    | 'connected'
    | 'peer-offline'
    | 'reconnecting'
    | 'error'

export type RemotePresenceState = 'offline' | PresenceState

/** 状态文案的键：偏好页与右键菜单共用同一套映射 */
export type PairStatusKey
  = | 'disabled'
    | 'disconnected'
    | 'connecting'
    | 'connected'
    | 'peerOffline'
    | 'error'

export function pairStatusKey(connection: PairConnectionState): PairStatusKey {
  switch (connection) {
    case 'connecting':
    case 'reconnecting':
      return 'connecting'
    case 'connected':
      return 'connected'
    case 'peer-offline':
      return 'peerOffline'
    case 'error':
      return 'error'
    case 'disabled':
      return 'disabled'
    case 'disconnected':
      return 'disconnected'
    default:
      return 'disconnected'
  }
}

/** 输入统计（§24 / §25）。只统计次数，不记录任何按键内容。 */
export interface PairStats {
  /** 计数对应的本地日期（yyyy-MM-dd）；跨过午夜先把今日计数归零 */
  date: string
  todayKeyboard: number
  todayMouse: number
  totalKeyboard: number
  totalMouse: number
}

/** §25 的线上载荷：在本地统计上多一个「对方是否愿意分享」的标记 */
export interface PairStatsPayload extends PairStats {
  share: boolean
}

export interface PairSettings {
  enabled: boolean

  /**
   * 本地暂离状态。
   *
   * §8 把它归在「运行时状态」里，但 tauri-pinia 只同步会被持久化的字段：暂离由偏好
   * 窗口触发、由猫咪窗口自动恢复，跨窗口必须一致，所以它放在设置里持久化。
   */
  presence: PresenceState

  relay: {
    url: string
    autoConnect: boolean
  }

  identity: {
    displayName: string
  }

  remoteCat: {
    visible: boolean
    scale: number
    opacity: number
    alwaysOnTop: boolean
    passThrough: boolean
    modelId?: string
    showStats: boolean
  }

  /** Phase 4（聊天）使用；先在 store 里按 §8 建好结构 */
  chat: {
    visible: boolean
    alwaysOnTop: boolean
    passThrough: boolean
    bubbleCount: number
    notificationSound: boolean
    notificationVolume: number
    /** §36 的本地保存上限，只用于提示进度，绝不静默删除消息 */
    historyMaxMessages: number
    /** 开始新的记录周期时，是否删除旧周期的消息（默认保留） */
    deleteOldOnReset: boolean
    /** §42 单个附件的上限（MB），同时约束发送与接收 */
    attachmentMaxMb: number
  }

  privacy: {
    shareTypingActivity: boolean
    sharePointer: boolean
    shareInputStats: boolean
    /** §52 暂停活动同步：暂停后不再发送宠物快照与统计，presence 仍然可用 */
    pauseActivitySync: boolean
  }

  away: {
    autoReturn: boolean
    message: string
    sendSystemNotice: boolean
  }
}

/** 连接相关的瞬时状态：不持久化，由 Tauri 事件与 `pair_get_status` 保持一致 */
export interface PairRuntime {
  connection: PairConnectionState
  /** P2P 这条腿：`off` / `connecting` / `connected`，纯显示用 */
  p2p: P2pState
  /**
   * 桌宠快照的发送上限（Hz），由 Rust 按当前生效传输给出（§6 / R23）。
   * `0` = 还没拿到：发送侧按 v1 的 3Hz 兜底（见 `usePairState`）。
   */
  petStateHz: number
  /** §23：服务器地址是不是明文（没有 TLS），只用来显示一条非阻塞提醒 */
  plaintext: boolean
  /**
   * 上面那条提醒说的是**哪一次连接**用的地址（Rust 侧只在 `start()` 里按当次地址算
   * `plaintext`）。界面拿它和输入框比一比，避免「改了地址、旧提醒还挂着」。
   */
  relayUrl?: string
  peerOnline: boolean
  peerName?: string
  remotePresence: RemotePresenceState
  remotePresenceMessage?: string
  remoteStats?: PairStatsPayload
  deviceId?: string
  lastError?: string
  /**
   * R39：正在猫咪窗口的聊天浮层里打字。
   *
   * 这个窗口的输入不该算成「猫的活动」：聚焦时既不发宠物快照，本机也不按贴图。
   * 只是本窗口的瞬时状态，不进设置、不落盘（`runtime` 本来就不持久化）。
   */
  localInputPaused: boolean
}

export const usePairStore = defineStore('pair', () => {
  const settings = reactive<PairSettings>({
    enabled: false,
    presence: 'active',
    relay: {
      url: '',
      autoConnect: false,
    },
    identity: {
      displayName: '',
    },
    remoteCat: {
      visible: false,
      scale: 100,
      opacity: 100,
      alwaysOnTop: true,
      passThrough: false,
      modelId: void 0,
      showStats: true,
    },
    chat: {
      visible: false,
      alwaysOnTop: true,
      passThrough: false,
      bubbleCount: 5,
      notificationSound: true,
      notificationVolume: 50,
      historyMaxMessages: 50_000,
      deleteOldOnReset: false,
      attachmentMaxMb: ATTACHMENT_MAX_MB.default,
    },
    privacy: {
      shareTypingActivity: true,
      sharePointer: true,
      // R38：默认开启（用户要求）——只发「今天/累计按了多少次」这种数字，不含内容
      shareInputStats: true,
      pauseActivitySync: false,
    },
    away: {
      autoReturn: true,
      message: '',
      sendSystemNotice: true,
    },
  })

  const stats = reactive<PairStats>({
    date: '',
    todayKeyboard: 0,
    todayMouse: 0,
    totalKeyboard: 0,
    totalMouse: 0,
  })

  const runtime = reactive<PairRuntime>({
    connection: 'disabled',
    p2p: 'off',
    petStateHz: 0,
    plaintext: false,
    relayUrl: void 0,
    peerOnline: false,
    peerName: void 0,
    remotePresence: 'offline',
    remotePresenceMessage: void 0,
    remoteStats: void 0,
    deviceId: void 0,
    lastError: void 0,
    localInputPaused: false,
  })

  /** 是否已经保存过 Pair Secret（明文永远不会进入这个 store） */
  const hasSecret = ref(false)
  /**
   * 是否已经保存过「服务器密码」（R36）。
   *
   * 与 `hasSecret` 分开：它只是服务器的门槛凭据，不是密钥材料，换掉它不会动到 E2EE。
   * 明文同样永远不会进入这个 store。
   */
  const hasServerPassword = ref(false)
  /** R17 的核对指纹，只在保存 secret 时由 Rust 回显 */
  const secretFingerprint = ref('')

  return {
    settings,
    stats,
    runtime,
    hasSecret,
    hasServerPassword,
    secretFingerprint,
  }
}, {
  tauri: {
    /**
     * §72：每个 WebView 都要初始化 pair store。
     *
     * 没有这一条时 store 只在当前窗口的内存里：设置与统计既不落盘，也不跨窗口同步，
     * 于是偏好窗口打开的开关（对方猫、暂停同步）在猫咪窗口里读不到，统计也永远显示 0。
     */
    autoStart: true,
    // 运行时状态与指纹不落盘：重新启动后必须先由 Rust 的真实状态覆盖，
    // 否则会先闪一下上一次会话的「已连接」
    filterKeys: ['runtime', 'hasSecret', 'secretFingerprint'],
  },
})

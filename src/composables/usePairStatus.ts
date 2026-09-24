import { onMounted } from 'vue'

import type { PairConnectionState } from '@/stores/pair'

import { LISTEN_KEY } from '@/constants'
import { usePairStore } from '@/stores/pair'

import type { PairStatus } from './usePair'

import { pairGetStatus } from './usePair'
import { useTauriListen } from './useTauriListen'

interface PeerChangedPayload {
  online: boolean
}

interface PresenceEventPayload {
  state: 'active' | 'away'
  message?: string
  displayName?: string
}

interface ErrorPayload {
  message: string
}

/**
 * 每个窗口都会调用一次：把 Rust 侧唯一的连接状态同步到这个窗口的 `runtime`。
 *
 * Leaf windows 不需要自己维护连接（网络只有一份，在 Rust 里），只要订阅事件即可，
 * 所以 remote-cat / preference / main 三个窗口看到的状态永远一致。
 */
export function usePairStatus() {
  const store = usePairStore()

  const translate = (state: PairStatus['state']): PairConnectionState => {
    switch (state) {
      case 'disabled':
        return 'disabled'
      case 'disconnected':
        return 'disconnected'
      case 'connecting':
        return 'connecting'
      case 'connected':
        return 'connected'
      case 'connected-peer-offline':
        return 'peer-offline'
      case 'reconnecting':
        return 'reconnecting'
      case 'error':
        return 'error'
      default:
        return 'disconnected'
    }
  }

  const applyStatus = (status: PairStatus) => {
    store.runtime.connection = store.settings.enabled ? translate(status.state) : 'disabled'
    store.runtime.p2p = store.settings.enabled ? (status.p2p ?? 'off') : 'off'
    store.runtime.petStateHz = store.settings.enabled ? (status.petStateHz ?? 0) : 0
    store.runtime.peerOnline = status.peerOnline
    store.runtime.peerName = status.peerName ?? void 0
    store.runtime.remotePresence = status.peerOnline
      ? (status.remotePresence ?? 'active')
      : 'offline'
    store.runtime.remoteStats = status.remoteStats ?? void 0
    store.runtime.lastError = status.lastError ?? void 0
    store.runtime.deviceId = status.deviceId
  }

  onMounted(async () => {
    applyStatus(await pairGetStatus())
  })

  useTauriListen<PairStatus>(LISTEN_KEY.PAIR_CONNECTION_CHANGED, ({ payload }) => {
    applyStatus(payload)
  })

  useTauriListen<PeerChangedPayload>(LISTEN_KEY.PAIR_PEER_CHANGED, ({ payload }) => {
    store.runtime.peerOnline = payload.online

    if (!payload.online) {
      store.runtime.remotePresence = 'offline'
      store.runtime.remotePresenceMessage = void 0
    }
  })

  useTauriListen<PresenceEventPayload>(LISTEN_KEY.PAIR_PRESENCE, ({ payload }) => {
    store.runtime.remotePresence = payload.state
    store.runtime.remotePresenceMessage = payload.message || void 0

    if (payload.displayName) {
      store.runtime.peerName = payload.displayName
    }
  })

  useTauriListen<NonNullable<PairStatus['remoteStats']>>(LISTEN_KEY.PAIR_STATS, ({ payload }) => {
    store.runtime.remoteStats = payload
  })

  useTauriListen<ErrorPayload>(LISTEN_KEY.PAIR_ERROR, ({ payload }) => {
    store.runtime.lastError = payload.message
  })
}

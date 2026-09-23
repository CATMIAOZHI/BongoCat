import { invoke } from '@tauri-apps/api/core'

import type { PairStatsPayload } from '@/stores/pair'

import { INVOKE_KEY } from '@/constants'

import type { PetSnapshot } from './usePairActivity'

export type PresenceState = 'active' | 'away'

/** `pair_get_status` / `pair-connection-changed` 的载荷（Rust 侧 `PairStatus`） */
export interface PairStatus {
  state: 'disabled' | 'disconnected' | 'connecting' | 'connected-peer-offline' | 'connected' | 'reconnecting' | 'error'
  peerOnline: boolean
  peerName?: string
  remotePresence?: PresenceState
  remoteStats?: PairStatsPayload
  deviceId: string
  relayUrl?: string
  lastError?: string
}

export interface PairPresencePayload {
  state: PresenceState
  message?: string
  displayName?: string
}

export function pairGetStatus() {
  return invoke<PairStatus>(INVOKE_KEY.PAIR_GET_STATUS)
}

export function pairGetDeviceId() {
  return invoke<string>(INVOKE_KEY.PAIR_GET_DEVICE_ID)
}

/** 保存 Pair Secret，返回 R17 的核对指纹（明文永不回显） */
export function pairSetSecret(secret: string) {
  return invoke<string>(INVOKE_KEY.PAIR_SET_SECRET, { secret })
}

export function pairHasSecret() {
  return invoke<boolean>(INVOKE_KEY.PAIR_HAS_SECRET)
}

/** 重启后重新读出已存 secret 的指纹；没存过时是 `null` */
export function pairGetSecretFingerprint() {
  return invoke<string | null>(INVOKE_KEY.PAIR_GET_SECRET_FINGERPRINT)
}

export function pairDeleteSecret() {
  return invoke<void>(INVOKE_KEY.PAIR_DELETE_SECRET)
}

export function pairConnect(relayUrl: string) {
  return invoke<void>(INVOKE_KEY.PAIR_CONNECT, { relayUrl })
}

export function pairDisconnect() {
  return invoke<void>(INVOKE_KEY.PAIR_DISCONNECT)
}

export function pairSendPresence(presence: PresenceState, message?: string, displayName?: string) {
  return invoke<void>(INVOKE_KEY.PAIR_SEND_PRESENCE, { presence, message, displayName })
}

export function pairSendPetState(snapshot: PetSnapshot) {
  return invoke<void>(INVOKE_KEY.PAIR_SEND_PET_STATE, { snapshot })
}

export function pairSendStats(stats: PairStatsPayload) {
  return invoke<void>(INVOKE_KEY.PAIR_SEND_STATS, { stats })
}

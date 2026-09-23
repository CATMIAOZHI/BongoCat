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

/** §31：单条文本消息的 UTF-8 上限，与 Rust 侧 `MESSAGE_TEXT_LIMIT` 一致 */
export const MESSAGE_TEXT_LIMIT = 8 * 1024

export type MessageDirection = 'incoming' | 'outgoing'

export type MessageKind = 'text' | 'image' | 'file' | 'voice'

export type MessageStatus = 'pending' | 'sent' | 'delivered' | 'failed' | 'received'

export type ExportFormat = 'json' | 'txt' | 'md'

/** 本地库里的一条聊天消息（Rust 侧 `ChatMessage`），`seq` 是分页游标 */
export interface ChatMessage {
  seq: number
  id: string
  direction: MessageDirection
  kind: MessageKind
  /** 毫秒时间戳 */
  createdAt: number
  text?: string
  status: MessageStatus
  attachmentId?: string
  conversationEpoch: number
}

/** 一页历史（§34）：`messages` 按时间升序，`hasMore` 表示还能往前翻 */
export interface HistoryPage {
  messages: ChatMessage[]
  hasMore: boolean
  epoch: number
}

/** §36 的本地保存进度 */
export interface HistoryStats {
  epoch: number
  current: number
  total: number
}

export interface ExportSummary {
  path: string
  format: ExportFormat
  messages: number
  exportedAt: number
}

/** 发一条文本消息（§31），返回值是本地已经落库的那一行 */
export function pairSendChat(text: string) {
  return invoke<ChatMessage>(INVOKE_KEY.PAIR_SEND_CHAT, { text })
}

/** 读一页历史；`before` 是游标（比它更旧的），不传就是最新一页 */
export function pairHistoryList(before?: number, limit?: number) {
  return invoke<HistoryPage>(INVOKE_KEY.PAIR_HISTORY_LIST, { before, limit })
}

export function pairHistoryStats() {
  return invoke<HistoryStats>(INVOKE_KEY.PAIR_HISTORY_STATS)
}

/** 导出到用户选定的路径（§35） */
export function pairHistoryExport(format: ExportFormat, path: string) {
  return invoke<ExportSummary>(INVOKE_KEY.PAIR_HISTORY_EXPORT, { format, path })
}

/** 导出并开始新的记录周期（§36）；`deleteOld` 为真时删除旧周期消息 */
export function pairHistoryStartNewEpoch(deleteOld?: boolean) {
  return invoke<number>(INVOKE_KEY.PAIR_HISTORY_START_NEW_EPOCH, { deleteOld })
}

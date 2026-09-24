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
  /** P2P 这条腿的状态（R21 / R28）：只用于显示，掉线不影响中继上的任何功能 */
  p2p?: P2pState
  /**
   * 前端该按多少 Hz 发桌宠快照（§6 / R23）。
   *
   * 上限由 Rust 按**当前生效传输**的额度算好：P2P 与「额度够的自建中继」是 60，
   * CF 缺省与其它情况是 3（v1 的老行为）。前端不再自己判断该用哪个上限。
   */
  petStateHz?: number
  /** §23：这次连接的地址是不是明文（`http://` / `ws://` / 裸 IP），只用于界面提醒 */
  plaintext?: boolean
}

export type P2pState = 'off' | 'connecting' | 'connected'

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

/** 生成一个新的联机密钥（§22）：Rust 侧用系统 CSPRNG 取 32 字节，前端不自己造 */
export function pairGenerateSecret() {
  return invoke<string>(INVOKE_KEY.PAIR_GENERATE_SECRET)
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

/** 附件类型（§37 / §38 / §44），对应 Rust 侧 `TransferKind` */
export type TransferKind = 'image' | 'file' | 'voice'

/**
 * 附件的本机记录（Rust 侧 `AttachmentRecord`，§39）。
 *
 * `localPath` 是本机的落盘位置（收到的附件在附件缓存里、发出的附件还在原处），
 * 永远不会发给对方；`size` / `sha256` 在传输完成前可能还没有。
 */
export interface AttachmentRecord {
  id: string
  kind: MessageKind
  originalName?: string
  mime?: string
  size?: number
  sha256?: string
  localPath?: string
  createdAt: number
}

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
  /** 附件消息带上附件记录（§37），文本消息没有这个字段 */
  attachment?: AttachmentRecord
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

/**
 * 一次附件传输在 UI 里的状态（Rust 侧 `TransferProgress` / `pair-transfer` 事件）。
 *
 * `waiting` 只有接收方会有：文件超过 50 MB 且不是图片 / 语音时先等用户点「接收」（§42）。
 */
export type TransferState = 'waiting' | 'sending' | 'receiving' | 'done' | 'failed' | 'canceled'

export interface TransferProgress {
  transferId: number
  messageId: string
  attachmentId: string
  kind: TransferKind
  name: string
  size: number
  transferred: number
  /** 0 - 100 */
  percent: number
  direction: MessageDirection
  state: TransferState
  /** 失败原因之类的补充说明 */
  message?: string
}

/** 附件与临时文件的落盘位置（Rust 侧 `TransferPaths`） */
export interface TransferPaths {
  root: string
  attachments: string
  tmp: string
  /** 单个附件上限（字节） */
  maxSize: number
}

/** §42：附件上限的可选区间（MB），与 Rust 侧的夹紧范围一致 */
export const ATTACHMENT_MAX_MB = {
  min: 1,
  max: 1024,
  default: 256,
} as const

export interface SendAttachmentOptions {
  path: string
  kind: TransferKind
  mime?: string
  /** 把源文件移进本机附件缓存；粘贴的图片和录音用它（§37） */
  stage?: boolean
}

export function pairTransferPaths() {
  return invoke<TransferPaths>(INVOKE_KEY.PAIR_TRANSFER_PATHS)
}

/** 设置单个附件的上限（MB），返回夹紧之后的字节数（§42） */
export function pairSetMaxAttachmentMb(mb: number) {
  return invoke<number>(INVOKE_KEY.PAIR_SET_MAX_ATTACHMENT_MB, { mb })
}

/** 发送一个附件（§38），返回值是本地已经落库的那条消息 */
export function pairSendAttachment(options: SendAttachmentOptions) {
  return invoke<ChatMessage>(INVOKE_KEY.PAIR_SEND_ATTACHMENT, {
    path: options.path,
    kind: options.kind,
    mime: options.mime,
    stage: options.stage,
  })
}

/** 接收方同意接收（§42 的大文件确认） */
export function pairTransferAccept(messageId: string) {
  return invoke<void>(INVOKE_KEY.PAIR_TRANSFER_ACCEPT, { messageId })
}

/** 接收方拒绝接收 */
export function pairTransferReject(messageId: string) {
  return invoke<void>(INVOKE_KEY.PAIR_TRANSFER_REJECT, { messageId })
}

/** 取消一次正在进行的传输 */
export function pairTransferCancel(messageId: string) {
  return invoke<void>(INVOKE_KEY.PAIR_TRANSFER_CANCEL, { messageId })
}

/** 重发一条失败的附件（§43）。对方发来的附件只能请对方重发。 */
export function pairAttachmentRetry(messageId: string) {
  return invoke<void>(INVOKE_KEY.PAIR_ATTACHMENT_RETRY, { messageId })
}

/** §45：单条语音的上限（秒），与 Rust 侧 `MAX_RECORDING_SECS` 一致 */
export const RECORDING_LIMIT_SECS = 60

/** §45 的按住说话：按下开始录，返回麦克风的原生采样率 */
export function pairStartRecording() {
  return invoke<number>(INVOKE_KEY.PAIR_START_RECORDING)
}

/**
 * §45 的按住说话：松开发送。
 *
 * 返回 `null` 表示这次没发出去：没在录，或者只轻点了一下（< 300 ms）。
 */
export function pairStopRecording() {
  return invoke<ChatMessage | null>(INVOKE_KEY.PAIR_STOP_RECORDING)
}

/** §45 的「可取消」：这次录音直接丢掉，不发送 */
export function pairCancelRecording() {
  return invoke<void>(INVOKE_KEY.PAIR_CANCEL_RECORDING)
}

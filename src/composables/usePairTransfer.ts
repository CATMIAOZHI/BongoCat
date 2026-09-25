import { ref } from 'vue'

import type {
  AttachmentRecord,
  ChatMessage,
  TransferProgress,
  TransferState,
} from './usePair'

import { pairAttachmentRetry, pairTransferAccept, pairTransferCancel, pairTransferReject } from './usePair'

/**
 * 附件传输在当前窗口的状态（§38 / §43）。
 *
 * 进度来自 Rust 侧的 `pair-transfer` 事件，只保存「这个窗口看到的最后一次进度」，
 * 所以不进 Pinia、不落盘：窗口重新加载后由后续的进度事件与聊天记录补上。
 */
const transfers = ref<Record<string, TransferProgress>>({})

/** 传输还在跑，进度条与「取消」要显示出来 */
export function isTransferActive(state: TransferState) {
  return state === 'waiting' || state === 'sending' || state === 'receiving'
}

/**
 * 接收方在等用户点「接收 / 拒绝」（§42 的 50 MB 以上普通文件）。
 *
 * 必须带上方向：发送方在等对方接收时也是 `waiting`，那边多出「接收 / 拒绝」按钮的话，
 * 自己一点就会把自己这次传输标成失败、还告诉对方「你拒绝了这次传输」。
 */
export function needsDecision(progress?: TransferProgress) {
  return progress?.state === 'waiting' && progress.direction === 'incoming'
}

/** 正在收或正在发，可以取消 */
export function canCancel(progress?: TransferProgress) {
  return progress?.state === 'sending' || progress?.state === 'receiving'
}

/**
 * 传输状态的文案键。
 *
 * `waiting` 在收发两侧含义不同：发送方在等对方点「接收」，接收方在等用户自己决定，
 * 所以拆成两个键，别让两边显示同一句话。
 */
export function transferLabelKey(progress: TransferProgress) {
  if (progress.state !== 'waiting') return progress.state

  return progress.direction === 'outgoing' ? 'waitingPeer' : 'waitingAccept'
}

/** 已经落到本机、可以「打开 / 另存为」的附件路径 */
export function localPathOf(attachment?: AttachmentRecord) {
  return attachment?.localPath || void 0
}

/** 能直接当图片渲染的附件路径：图片且已经落到本机 */
export function previewableImage(item: ChatMessage) {
  const attachment = item.attachment

  if (attachment?.kind !== 'image') return void 0

  return localPathOf(attachment)
}

/** 附件在气泡里显示的标题，取对方给的原文件名 */
export function attachmentTitle(attachment?: AttachmentRecord) {
  return attachment?.originalName || void 0
}

/** 人类可读的文件大小，例如 1.5 MB；只用于显示 */
export function formatFileSize(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'

  const units = ['B', 'KB', 'MB', 'GB']
  let value = bytes
  let index = 0

  while (value >= 1024 && index < units.length - 1) {
    value /= 1024
    index += 1
  }

  // 整数字节与三位数以上不再保留小数，读起来更干净
  const rounded = index === 0 || value >= 100 ? Math.round(value) : Math.round(value * 10) / 10

  return `${rounded} ${units[index]}`
}

export function extensionOf(name: string) {
  const dot = name.lastIndexOf('.')

  if (dot <= 0 || dot === name.length - 1) return ''

  return name.slice(dot + 1).toLowerCase()
}

export function usePairTransfer() {
  /** 一条消息对应的传输进度 */
  function transferOf(messageId: string) {
    return transfers.value[messageId]
  }

  /** 收下一条进度；内容没变时不触发重渲染 */
  function apply(progress: TransferProgress) {
    const current = transfers.value[progress.messageId]

    if (
      current
      && current.transferId === progress.transferId
      && current.state === progress.state
      && current.transferred === progress.transferred
    ) {
      return
    }

    transfers.value = { ...transfers.value, [progress.messageId]: progress }
  }

  function reset() {
    transfers.value = {}
  }

  return {
    transfers,
    transferOf,
    apply,
    reset,
  }
}

/** 接收方同意接收 */
export function acceptTransfer(messageId: string) {
  return pairTransferAccept(messageId)
}

export function rejectTransfer(messageId: string) {
  return pairTransferReject(messageId)
}

export function cancelTransfer(messageId: string) {
  return pairTransferCancel(messageId)
}

/** 重发一条失败的附件（§43） */
export function retryTransfer(messageId: string) {
  return pairAttachmentRetry(messageId)
}

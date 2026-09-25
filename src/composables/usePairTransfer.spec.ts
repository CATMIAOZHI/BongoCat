import { describe, expect, it } from 'vitest'

import type { AttachmentRecord, ChatMessage, TransferProgress } from './usePair'

import {
  attachmentTitle,
  canCancel,
  formatFileSize,
  isTransferActive,
  localPathOf,
  needsDecision,
  previewableImage,
  transferLabelKey,
} from './usePairTransfer'

function attachment(overrides: Partial<AttachmentRecord> = {}): AttachmentRecord {
  return {
    id: 'attachment-1',
    kind: 'image',
    originalName: 'cat.png',
    mime: 'image/png',
    size: 1024,
    createdAt: 1_700_000_000_000,
    ...overrides,
  }
}

function message(overrides: Partial<ChatMessage> = {}): ChatMessage {
  return {
    seq: 1,
    id: 'id-1',
    direction: 'incoming',
    kind: 'image',
    createdAt: 1_700_000_000_000,
    status: 'received',
    conversationEpoch: 1,
    ...overrides,
  }
}

function progress(state: TransferProgress['state']): TransferProgress {
  return {
    transferId: 7,
    messageId: 'id-1',
    attachmentId: 'attachment-1',
    kind: 'file',
    name: 'cat.zip',
    size: 1024,
    transferred: 0,
    percent: 0,
    direction: 'incoming',
    state,
  }
}

describe('进度条与按钮只在传输没结束时显示', () => {
  it('等待确认、发送中、接收中都算还在跑', () => {
    expect(isTransferActive('waiting')).toBe(true)
    expect(isTransferActive('sending')).toBe(true)
    expect(isTransferActive('receiving')).toBe(true)
  })

  it('结束状态不再显示进度条', () => {
    expect(isTransferActive('done')).toBe(false)
    expect(isTransferActive('failed')).toBe(false)
    expect(isTransferActive('canceled')).toBe(false)
  })

  it('只有等待确认时才要「接收 / 拒绝」', () => {
    expect(needsDecision({ ...progress('waiting'), direction: 'incoming' })).toBe(true)
    expect(needsDecision({ ...progress('receiving'), direction: 'incoming' })).toBe(false)
    expect(needsDecision()).toBe(false)
  })

  it('自己发出去的附件不给「接收 / 拒绝」，那是对方的事', () => {
    expect(needsDecision({ ...progress('waiting'), direction: 'outgoing' })).toBe(false)
  })

  it('只有正在收发时才给「取消」', () => {
    expect(canCancel(progress('sending'))).toBe(true)
    expect(canCancel(progress('receiving'))).toBe(true)
    expect(canCancel(progress('waiting'))).toBe(false)
    expect(canCancel(progress('done'))).toBe(false)
  })
})

describe('等待确认在收发两侧不是同一句话', () => {
  it('发送方在等对方点接收', () => {
    expect(transferLabelKey({ ...progress('waiting'), direction: 'outgoing' })).toBe('waitingPeer')
  })

  it('接收方在等用户自己决定', () => {
    expect(transferLabelKey({ ...progress('waiting'), direction: 'incoming' })).toBe('waitingAccept')
  })

  it('其余状态直接用状态名', () => {
    expect(transferLabelKey(progress('sending'))).toBe('sending')
    expect(transferLabelKey(progress('failed'))).toBe('failed')
  })
})

describe('文件大小只用于显示', () => {
  it('一字节以内不乱报', () => {
    expect(formatFileSize(0)).toBe('0 B')
    expect(formatFileSize(-1)).toBe('0 B')
    expect(formatFileSize(Number.NaN)).toBe('0 B')
  })

  it('按 B / KB / MB / GB 递进', () => {
    expect(formatFileSize(512)).toBe('512 B')
    expect(formatFileSize(1024)).toBe('1 KB')
    expect(formatFileSize(1536)).toBe('1.5 KB')
    expect(formatFileSize(256 * 1024 * 1024)).toBe('256 MB')
    expect(formatFileSize(1024 * 1024 * 1024)).toBe('1 GB')
  })
})

describe('附件气泡取的是本机路径', () => {
  it('图片且已经落盘才能直接预览', () => {
    const image = message({ attachment: attachment({ localPath: 'C:\\cache\\a.png' }) })

    expect(previewableImage(image)).toBe('C:\\cache\\a.png')
    expect(localPathOf(image.attachment)).toBe('C:\\cache\\a.png')
  })

  it('还没收到、或者不是图片时不给预览', () => {
    expect(previewableImage(message({ attachment: attachment() }))).toBeUndefined()

    const file = message({
      kind: 'file',
      attachment: attachment({ kind: 'file', localPath: 'C:\\cache\\a.zip' }),
    })

    expect(previewableImage(file)).toBeUndefined()
    // 「打开 / 另存为」对普通文件仍然可用
    expect(localPathOf(file.attachment)).toBe('C:\\cache\\a.zip')
  })

  it('没有原文件名时不硬造一个名字', () => {
    expect(attachmentTitle(attachment())).toBe('cat.png')
    expect(attachmentTitle(attachment({ originalName: void 0 }))).toBeUndefined()
    expect(attachmentTitle()).toBeUndefined()
  })
})

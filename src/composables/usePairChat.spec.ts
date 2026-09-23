import { describe, expect, it } from 'vitest'

import type { ChatMessage } from './usePair'

import {
  chatExportFileName,
  formatClock,
  mergeMessages,
  sortMessages,
  upsertMessage,
  visibleWindow,
} from './usePairChat'

function message(seq: number, overrides: Partial<ChatMessage> = {}): ChatMessage {
  return {
    seq,
    id: `id-${seq}`,
    direction: 'outgoing',
    kind: 'text',
    createdAt: 1_700_000_000_000 + seq,
    text: `第 ${seq} 条`,
    status: 'pending',
    conversationEpoch: 1,
    ...overrides,
  }
}

describe('气泡窗口显示的是最新的一屏', () => {
  it('不足一屏时全都显示', () => {
    expect(visibleWindow(3, 5, 0)).toEqual({ start: 0, end: 3 })
  })

  it('只看最新 N 条', () => {
    expect(visibleWindow(100, 5, 0)).toEqual({ start: 95, end: 100 })
  })

  it('往回看历史时窗口整体前移', () => {
    expect(visibleWindow(100, 5, 3)).toEqual({ start: 92, end: 97 })
  })

  it('偏移超出范围时停在最旧的一屏，不会算出空窗口', () => {
    expect(visibleWindow(100, 5, 999)).toEqual({ start: 0, end: 5 })
    expect(visibleWindow(3, 5, 999)).toEqual({ start: 0, end: 3 })
  })

  it('气泡数量的下限是 1', () => {
    expect(visibleWindow(10, 0, 0)).toEqual({ start: 9, end: 10 })
  })
})

describe('消息列表的合并', () => {
  it('新消息按 seq 插到对应位置', () => {
    const list = sortMessages([message(3), message(1)])

    expect(list.map(item => item.seq)).toEqual([1, 3])

    const merged = upsertMessage(list, message(2))

    expect(merged.map(item => item.seq)).toEqual([1, 2, 3])
  })

  it('同一条消息只留一份，状态回执就地更新', () => {
    const list = [message(1), message(2)]
    const updated = upsertMessage(list, { ...message(2), status: 'delivered' })

    expect(updated).toHaveLength(2)
    expect(updated[1].status).toBe('delivered')
  })

  it('内容没有变化时返回原数组，避免多余的重渲染', () => {
    const list = [message(1), message(2)]

    expect(upsertMessage(list, message(2))).toBe(list)
    expect(upsertMessage(list, { ...message(2), text: '改过' })).not.toBe(list)
  })

  it('一页历史并入时跨页重复的 id 不会变成两条', () => {
    const list = [message(3)]
    const page = [message(1), message(2), message(3)]
    const merged = mergeMessages(list, page)

    expect(merged.map(item => item.id)).toEqual(['id-1', 'id-2', 'id-3'])
  })
})

describe('时间与导出文件名', () => {
  it('只显示时分，且不随后端语言变化', () => {
    expect(formatClock(new Date(2026, 8, 23, 8, 5).getTime())).toBe('08:05')
    expect(formatClock(new Date(2026, 8, 23, 23, 59).getTime())).toBe('23:59')
  })

  it('默认文件名带日期和格式后缀', () => {
    const now = new Date(2026, 8, 23, 10, 0).getTime()

    expect(chatExportFileName('json', now)).toBe('bongocat-chat-2026-09-23.json')
    expect(chatExportFileName('md', now)).toBe('bongocat-chat-2026-09-23.md')
  })
})

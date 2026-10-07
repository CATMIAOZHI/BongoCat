import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { ChatMessage } from './usePair'

const api = vi.hoisted(() => ({ pairHistoryList: vi.fn(), pairSendChat: vi.fn() }))
vi.mock('./usePair', () => api)

function page(seq: number, hasMore = true) {
  const message: ChatMessage = {
    id: `id-${seq}`,
    seq,
    createdAt: 0,
    text: 'hello',
    kind: 'text',
    direction: 'incoming',
    status: 'received',
    conversationEpoch: 1,
  }
  return { messages: [message], hasMore }
}

describe('聊天历史分页竞争', () => {
  beforeEach(() => {
    vi.resetModules()
    api.pairHistoryList.mockReset()
  })

  it('清空历史后，迟到的旧分页不能把旧消息带回来', async () => {
    const { usePairChat } = await import('./usePairChat')
    const chat = usePairChat()
    api.pairHistoryList.mockResolvedValueOnce(page(50))
    await chat.loadLatest()
    let finish!: (value: ReturnType<typeof page>) => void
    api.pairHistoryList.mockImplementationOnce(() => new Promise((resolve) => {
      finish = resolve
    }))
    const beforeMerge = vi.fn()
    const older = chat.loadOlder(beforeMerge)
    api.pairHistoryList.mockResolvedValueOnce(page(100, false))
    await chat.loadLatest()
    finish(page(1))
    expect(await older).toBe(0)
    expect(chat.messages.value.map(item => item.seq)).toEqual([100])
    expect(beforeMerge).not.toHaveBeenCalled()
    expect(chat.hasMore.value).toBe(false)
  })

  it('分页等待期间收到的新消息保留，锚点在合并前读取，重复翻页不并发', async () => {
    const { usePairChat } = await import('./usePairChat')
    const chat = usePairChat()
    api.pairHistoryList.mockResolvedValueOnce(page(50))
    await chat.loadLatest()
    let finish!: (value: ReturnType<typeof page>) => void
    api.pairHistoryList.mockImplementationOnce(() => new Promise((resolve) => {
      finish = resolve
    }))
    const beforeMerge = vi.fn(() => {
      expect(chat.messages.value.map(item => item.seq)).toEqual([50, 51])
    })
    const older = chat.loadOlder(beforeMerge)
    expect(await chat.loadOlder()).toBe(0)
    chat.apply(page(51).messages[0])
    finish(page(1, false))
    expect(await older).toBe(1)
    expect(beforeMerge).toHaveBeenCalledOnce()
    expect(chat.messages.value.map(item => item.seq)).toEqual([1, 50, 51])
    expect(api.pairHistoryList).toHaveBeenCalledTimes(2)
  })
})

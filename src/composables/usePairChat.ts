import { ref } from 'vue'

import type { ChatMessage, ExportFormat } from './usePair'

import { pairHistoryList, pairSendChat } from './usePair'

/** 一页历史的条数（§34）：滚到顶部再往前翻同样这么多 */
export const PAGE_SIZE = 50

/**
 * 当前窗口里的消息列表（§33）。
 *
 * 聊天历史永远以 Rust 侧的 SQLite 为准，这里只保存「这个窗口已经读出来的那一部分」，
 * 所以它不进 Pinia、不落盘：窗口重新加载后重新拉取即可。
 */
const messages = ref<ChatMessage[]>([])
const hasMore = ref(false)
const loading = ref(false)

/** 按 `seq`（本地自增序号）升序排列，越旧越靠前 */
export function sortMessages(list: ChatMessage[]) {
  return [...list].sort((a, b) => a.seq - b.seq)
}

/**
 * 把一条消息并进列表：同 id 的消息只留一份（对端重发、状态回执都走这里）。
 *
 * 内容完全没变时返回原数组，避免无意义的重渲染。
 */
export function upsertMessage(list: ChatMessage[], message: ChatMessage) {
  const index = list.findIndex(item => item.id === message.id)

  if (index < 0) {
    return sortMessages([...list, message])
  }

  const previous = list[index]

  if (
    previous.seq === message.seq
    && previous.status === message.status
    && previous.text === message.text
  ) {
    return list
  }

  const next = [...list]

  next[index] = message

  return next
}

/** 把一页历史并进现有列表，跨页重复的 id 只留一份 */
export function mergeMessages(list: ChatMessage[], incoming: ChatMessage[]) {
  return incoming.reduce(upsertMessage, list)
}

/**
 * 气泡窗口当前要渲染的消息区间 `[start, end)`（§29）。
 *
 * `count` 是同时显示的气泡数量（`bubbleCount`），`offset` 是从最新一条往回数的条数：
 * `offset` 为 0 时看的是最新一屏，往前往回看历史时它才会增长。
 */
export function visibleWindow(total: number, count: number, offset: number) {
  const visible = Math.max(1, count)
  const maxOffset = Math.max(0, total - visible)
  const safeOffset = Math.min(Math.max(0, offset), maxOffset)
  const end = total - safeOffset
  const start = Math.max(0, end - visible)

  return { start, end }
}

/** 气泡里的时间，只显示时分 */
export function formatClock(value: number) {
  const date = new Date(value)
  const pad = (part: number) => String(part).padStart(2, '0')

  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** 导出文件的默认文件名，例如 bongocat-chat-2026-09-23.json */
export function chatExportFileName(format: ExportFormat, now: number) {
  const date = new Date(now)
  const pad = (part: number) => String(part).padStart(2, '0')
  const stamp = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`

  return `bongocat-chat-${stamp}.${format}`
}

export function usePairChat() {
  /** 读最新一页：窗口打开、以及开始新周期之后都走这里 */
  async function loadLatest() {
    loading.value = true

    try {
      const page = await pairHistoryList(void 0, PAGE_SIZE)

      messages.value = sortMessages(page.messages)
      hasMore.value = page.hasMore
    } finally {
      loading.value = false
    }
  }

  /** 往前翻一页，返回真正新增的条数（调用方用它保持画面不动） */
  async function loadOlder() {
    if (loading.value || !hasMore.value) return 0

    const oldest = messages.value[0]

    if (!oldest) return 0

    loading.value = true

    try {
      const page = await pairHistoryList(oldest.seq, PAGE_SIZE)
      const before = messages.value.length

      messages.value = mergeMessages(messages.value, page.messages)
      hasMore.value = page.hasMore

      return messages.value.length - before
    } finally {
      loading.value = false
    }
  }

  async function send(text: string) {
    const message = await pairSendChat(text)

    messages.value = upsertMessage(messages.value, message)

    return message
  }

  /** 收到新消息或状态回执时把列表更新掉 */
  function apply(message: ChatMessage) {
    messages.value = upsertMessage(messages.value, message)
  }

  return {
    messages,
    hasMore,
    loading,
    loadLatest,
    loadOlder,
    send,
    apply,
  }
}

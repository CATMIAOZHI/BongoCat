<script setup lang="ts">
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { writeText } from '@tauri-apps/plugin-clipboard-manager'
import { error } from '@tauri-apps/plugin-log'
import { useEventListener } from '@vueuse/core'
import { computed, nextTick, onMounted, ref, useTemplateRef, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage, MessageStatus } from '@/composables/usePair'

import { MESSAGE_TEXT_LIMIT } from '@/composables/usePair'
import { formatClock, usePairChat, visibleWindow } from '@/composables/usePairChat'
import { usePairStatus } from '@/composables/usePairStatus'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, setAlwaysOnTop, showWindowByLabel } from '@/plugins/window'
import { usePairStore } from '@/stores/pair'

/**
 * 桌面聊天气泡窗口（§29 - §32）。
 *
 * 只负责三件事：显示最新的一屏消息、按需往前翻历史、进入/退出输入模式。
 * 通知动画（Q 弹、闪光、提示音）在对方猫窗口里做（§47），这里不碰。
 */

/** 状态图标（§31） */
const STATUS_ICON: Record<MessageStatus, string> = {
  pending: 'i-lucide:clock',
  sent: 'i-lucide:check',
  delivered: 'i-lucide:check-check',
  failed: 'i-lucide:triangle-alert',
  received: 'i-lucide:check',
}

const appWindow = getCurrentWebviewWindow()
const pairStore = usePairStore()
const { t } = useI18n()
const { messages, loading, loadLatest, loadOlder, send, apply } = usePairChat()
const listRef = useTemplateRef<HTMLElement>('list')
const inputRef = useTemplateRef<HTMLTextAreaElement>('input')

/** 从最新一条往回数的条数：0 就是「看最新的一屏」 */
const offset = ref(0)
const inputMode = ref(false)
const draft = ref('')
const sending = ref(false)
const sendError = ref('')
const copiedId = ref('')
let copiedTimer: ReturnType<typeof setTimeout> | undefined

usePairStatus()

const bubbleCount = computed(() => {
  const value = Math.round(Number(pairStore.settings.chat.bubbleCount))

  return Math.min(20, Math.max(1, Number.isFinite(value) ? value : 5))
})

const visibleMessages = computed(() => {
  const { start, end } = visibleWindow(messages.value.length, bubbleCount.value, offset.value)

  return messages.value.slice(start, end)
})

const atNewest = computed(() => offset.value === 0)

/** 输入的内容按 UTF-8 字节算，和 Rust 侧的限制对齐（§31） */
const draftBytes = computed(() => new TextEncoder().encode(draft.value).length)
const tooLong = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT)
const showingLimit = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT * 0.8)

const peerTitle = computed(() => {
  if (!pairStore.settings.enabled) return t('pages.chat.hints.disabled')
  if (!pairStore.runtime.peerOnline) return t('pages.chat.hints.offline')

  return pairStore.runtime.peerName || t('pages.chat.labels.peer')
})

/** 穿透开着时点不到窗口，提示一句，免得用户以为窗口坏了 */
const footerHint = computed(() => {
  if (pairStore.settings.chat.passThrough && !inputMode.value) {
    return t('pages.chat.hints.passThrough')
  }

  return t('pages.chat.hints.inputHint')
})

function statusTitle(status: MessageStatus) {
  return t(`pages.chat.status.${status}`)
}

/** 输入模式下临时关掉穿透，退出时恢复用户原本的设置（§30） */
const ignoreCursor = computed(() => pairStore.settings.chat.passThrough && !inputMode.value)

watch(ignoreCursor, (value) => {
  appWindow.setIgnoreCursorEvents(value)
}, { immediate: true })

watch(() => pairStore.settings.chat.alwaysOnTop, setAlwaysOnTop, { immediate: true })

function openInput() {
  inputMode.value = true

  nextTick(() => inputRef.value?.focus())
}

function closeInput() {
  inputMode.value = false
  inputRef.value?.blur()
}

function toggleInput() {
  if (inputMode.value) {
    closeInput()

    return
  }

  openInput()
}

watch(() => pairStore.settings.chat.visible, (visible) => {
  if (visible) {
    showWindowByLabel(WINDOW_LABEL.CHAT).catch(reason => error(String(reason)))

    return
  }

  closeInput()
  hideWindowByLabel(WINDOW_LABEL.CHAT).catch(reason => error(String(reason)))
}, { immediate: true })

async function scrollToNewest() {
  await nextTick()

  const list = listRef.value

  if (list) list.scrollTop = list.scrollHeight
}

/** 滚轮在边界上继续滚时，一次往前/往后挪一条（§29 的「滚轮查看历史」） */
async function stepOlder() {
  const maxOffset = Math.max(0, messages.value.length - bubbleCount.value)

  if (offset.value < maxOffset) {
    offset.value += 1

    return
  }

  const loadedCount = await loadOlder().catch((reason) => {
    error(String(reason))

    return 0
  })

  // 新读出来的更旧消息都算进「已经回看」的范围，画面因此不会跳
  if (loadedCount > 0) offset.value += loadedCount
}

function handleWheel(event: WheelEvent) {
  const list = listRef.value

  if (!list) return

  if (event.deltaY < 0) {
    if (list.scrollTop > 0) return

    event.preventDefault()
    void stepOlder()

    return
  }

  if (list.scrollTop + list.clientHeight < list.scrollHeight - 1) return

  event.preventDefault()
  offset.value = Math.max(0, offset.value - 1)
}

async function handleSend() {
  if (!draft.value.trim() || tooLong.value || sending.value) return

  sending.value = true
  sendError.value = ''

  try {
    // 连不上时这条消息留在本地队列里（§32），状态显示为「等待发送」
    await send(draft.value)
    draft.value = ''
    offset.value = 0
    await scrollToNewest()
  } catch (reason) {
    sendError.value = String(reason)
  } finally {
    sending.value = false
  }
}

/**
 * Enter 发送、Shift+Enter 换行。
 *
 * 中文/日文输入法里「确认候选词」也是 Enter，此时 `event.isComposing` 为真，绝不能当成
 * 发送；所以这里自己 `preventDefault`，而不是用 `.prevent` 修饰符（那会在判定前就吃掉按键）。
 * 个别 Chromium 版本在「结束合成的那一次 Enter」上给的是 `isComposing === false` +
 * `keyCode === 229`，所以再补一条兜底。
 */
function handleSendKey(event: KeyboardEvent) {
  if (event.isComposing || event.keyCode === 229) return

  event.preventDefault()
  void handleSend()
}

async function handleCopy(message: ChatMessage) {
  try {
    await writeText(message.text ?? '')
    copiedId.value = message.id

    if (copiedTimer) clearTimeout(copiedTimer)

    copiedTimer = setTimeout(() => {
      copiedId.value = ''
    }, 1200)
  } catch (reason) {
    error(String(reason))
  }
}

function handleHide() {
  pairStore.settings.chat.visible = false
}

async function backToNewest() {
  offset.value = 0

  await scrollToNewest()
}

function handleMouseDown(event: MouseEvent) {
  if ((event.target as HTMLElement).closest('button')) return

  appWindow.startDragging()
}

useTauriListen(LISTEN_KEY.CHAT_INPUT_TOGGLE, toggleInput)

useTauriListen(LISTEN_KEY.CHAT_HISTORY_RESET, () => {
  offset.value = 0

  void loadLatest().then(scrollToNewest).catch((reason) => {
    error(String(reason))
  })
})

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, ({ payload }) => {
  apply(payload)

  if (atNewest.value) void scrollToNewest()
})

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_UPDATED, ({ payload }) => {
  apply(payload)
})

useEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeInput()
})

onMounted(async () => {
  try {
    await loadLatest()
    await scrollToNewest()
  } catch (reason) {
    error(String(reason))
  }
})
</script>

<template>
  <div class="size-screen flex flex-col overflow-hidden bg-black/45 text-white rounded-2xl">
    <header
      class="flex shrink-0 cursor-move items-center gap-1.5 px-2.5 py-1.5"
      @mousedown="handleMouseDown"
    >
      <span class="i-lucide:message-circle shrink-0 text-[13px] color-white/50" />

      <span class="min-w-0 flex-1 truncate text-[11px] color-white/55">
        {{ peerTitle }}
      </span>

      <button
        v-if="!atNewest"
        class="i-lucide:chevron-down shrink-0 cursor-pointer text-[14px] color-white/60 hover:color-white"
        :title="$t('pages.chat.hints.backToNewest')"
        @click="backToNewest"
      />

      <button
        class="i-lucide:x shrink-0 cursor-pointer text-[14px] color-white/60 hover:color-white"
        :title="$t('pages.chat.hints.hide')"
        @click="handleHide"
      />
    </header>

    <div
      ref="list"
      class="min-h-0 flex-1 overflow-y-auto px-2 pb-1.5"
      @wheel="handleWheel"
    >
      <div
        v-if="loading && !messages.length"
        class="py-4 text-center text-[10px] color-white/40"
      >
        {{ $t('pages.chat.hints.loading') }}
      </div>

      <div
        v-else-if="!messages.length"
        class="py-4 text-center text-[10px] color-white/40"
      >
        {{ $t('pages.chat.hints.empty') }}
      </div>

      <div
        v-for="item in visibleMessages"
        :key="item.id"
        class="group mb-1.5 flex"
        :class="item.direction === 'outgoing' ? 'justify-end' : 'justify-start'"
      >
        <div
          class="max-w-[86%] px-2 py-1 text-[12px] leading-[1.35] rounded-xl"
          :class="item.direction === 'outgoing'
            ? 'bg-[#1677ff] rounded-br-sm'
            : 'bg-white/15 rounded-bl-sm'"
        >
          <p class="whitespace-pre-wrap break-all">
            {{ item.text }}
          </p>

          <div class="mt-0.5 flex items-center justify-end gap-1 text-[9px] color-white/55">
            <button
              class="shrink-0 cursor-pointer text-[10px] opacity-0 transition group-hover:opacity-100 hover:color-white"
              :class="copiedId === item.id ? 'i-lucide:check' : 'i-lucide:copy'"
              :title="$t('pages.chat.hints.copy')"
              @click="handleCopy(item)"
            />

            <span>{{ formatClock(item.createdAt) }}</span>

            <span
              v-if="item.direction === 'outgoing'"
              class="shrink-0 text-[10px]"
              :class="[STATUS_ICON[item.status], item.status === 'failed' ? 'color-red-3' : 'color-white/70']"
              :title="statusTitle(item.status)"
            />
          </div>
        </div>
      </div>
    </div>

    <div
      v-if="inputMode"
      class="shrink-0 border-t border-white/10 p-1.5"
    >
      <textarea
        ref="input"
        v-model="draft"
        class="h-10 w-full resize-none bg-white/10 px-2 py-1 text-[12px] outline-none rounded-lg placeholder:color-white/35"
        :placeholder="$t('pages.chat.placeholders.input')"
        @keydown.enter.exact="handleSendKey"
        @keydown.esc.prevent="closeInput"
      />

      <div class="mt-1 flex items-center justify-between gap-2 text-[9px] color-white/45">
        <span class="min-w-0 truncate">{{ $t('pages.chat.hints.inputKeys') }}</span>

        <span
          v-if="showingLimit"
          :class="tooLong ? 'color-red-3' : ''"
        >
          {{ $t('pages.chat.hints.textLimit', { bytes: draftBytes }) }}
        </span>
      </div>

      <p
        v-if="sendError"
        class="mt-1 break-all text-[9px] color-red-3"
      >
        {{ sendError }}
      </p>
    </div>

    <div
      v-else
      class="shrink-0 px-2.5 pb-1.5"
    >
      <button
        class="cursor-pointer text-[9px] color-white/35 hover:color-white/70"
        @click="openInput"
      >
        {{ footerHint }}
      </button>
    </div>
  </div>
</template>

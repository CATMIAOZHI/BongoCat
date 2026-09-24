<script setup lang="ts">
import { computed, onMounted, ref, useTemplateRef } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage } from '@/composables/usePair'

import { MESSAGE_TEXT_LIMIT, RECORDING_LIMIT_SECS } from '@/composables/usePair'
import { usePairChat } from '@/composables/usePairChat'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY } from '@/constants'
import { usePairStore } from '@/stores/pair'

/**
 * 猫咪窗口上的聊天浮层（R39）。
 *
 * 两件事：显示最新一屏消息（条数沿用「同时显示的气泡数」设置），以及一条常驻输入条
 * （麦克风 + 输入框 + 发送）。消息仍然以 Rust 侧 SQLite 为准，这里只保存本窗口读出来的
 * 那一部分，和聊天窗口是同一套读取方式。
 *
 * 在这里打字**也算猫的活动**（用户要求）：浮层不做任何「暂停同步」的动作，全局键鼠监听
 * 照旧把这段输入喂给本机贴图与对方，所以自己的猫和对方的猫都会跟着按。
 */
const props = defineProps<{
  /** 是否正在录音。录音实体在父窗口那一份会话里，这里只负责显示与开合 */
  recording: boolean
  recordingSeconds: number
  /** R41：有一段录好、还没确认发送的语音（确认按钮在猫咪窗口下方那条提示上） */
  pending: boolean
}>()

const emit = defineEmits<{
  voiceStart: []
  voiceStop: []
}>()

/**
 * 浮层里同时显示的气泡上限（R39）。
 *
 * 比聊天窗口的「同时显示的气泡数」更小：猫咪窗口顶上只有一条窄带，按默认的 5 条会顶到
 * 上沿被裁掉半个气泡。取小的一侧，多出来的消息去聊天窗口看。
 */
const OVERLAY_BUBBLE_MAX = 3

const pairStore = usePairStore()
const { t } = useI18n()
const { messages, loadLatest, apply, send } = usePairChat()

const inputRef = useTemplateRef<HTMLTextAreaElement>('input')
const draft = ref('')
const sending = ref(false)
const sendError = ref('')

const online = computed(() => pairStore.settings.enabled && pairStore.runtime.peerOnline)

/** 同时显示的气泡数：与聊天窗口共用同一个设置 */
const bubbleCount = computed(() => {
  const value = Math.round(Number(pairStore.settings.chat.bubbleCount))

  return Math.min(20, Math.max(1, Number.isFinite(value) ? value : 5))
})

const bubbles = computed(() => messages.value.slice(-Math.min(bubbleCount.value, OVERLAY_BUBBLE_MAX)))

const draftBytes = computed(() => new TextEncoder().encode(draft.value).length)
const tooLong = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT)
const canSend = computed(() => {
  return online.value && !sending.value && !tooLong.value && draft.value.trim().length > 0
})

/** 输入框里的提示行：录音中 > 待发送的录音 > 没连线 > 发送失败 */
const hint = computed(() => {
  if (props.recording) {
    return t('pages.main.hints.recording', { seconds: props.recordingSeconds, limit: RECORDING_LIMIT_SECS })
  }

  // R41：录完先不发送，提示去下面那条「试听 / 发送 / 取消」上确认
  if (props.pending) return t('pages.chat.hints.voiceReady')

  if (!pairStore.settings.enabled) return t('pages.chat.hints.disabled')
  if (!pairStore.runtime.peerOnline) return t('pages.chat.hints.offline')

  return sendError.value
})

/** 附件消息在气泡里只显示一个短标签，正文仍然是 `text` */
function bubbleText(message: ChatMessage) {
  if (message.text) return message.text
  if (message.kind === 'image') return t('pages.main.hints.bubbleImage')
  if (message.kind === 'voice') return t('pages.main.hints.bubbleVoice')

  return t('pages.main.hints.bubbleFile')
}

async function handleSend() {
  if (!canSend.value) return

  const text = draft.value.trim()

  sending.value = true
  sendError.value = ''

  try {
    await send(text)

    draft.value = ''
  } catch (reason) {
    sendError.value = reason instanceof Error ? reason.message : String(reason)
  } finally {
    sending.value = false
  }
}

/** Enter 发送、Shift+Enter 换行、Esc 退出输入（与聊天窗口同一套键位） */
function handleKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault()
    inputRef.value?.blur()

    return
  }

  if (event.key !== 'Enter' || event.shiftKey) return

  event.preventDefault()

  void handleSend()
}

/**
 * 麦克风：点一下开始，再点一下结束（R41 起结束只是**录好待确认**，不再直接发送）。
 *
 * 已经有一条待确认的录音时再点麦克风，就是重录一条：Rust 侧会把上一条的临时文件删掉。
 * 录音期间把焦点交还出去，免得打字又被当成按键。
 */
function handleVoice() {
  inputRef.value?.blur()

  if (props.recording) {
    emit('voiceStop')

    return
  }

  emit('voiceStart')
}

onMounted(() => {
  void loadLatest().catch(() => void 0)
})

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, ({ payload }) => apply(payload))

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_UPDATED, ({ payload }) => apply(payload))

useTauriListen(LISTEN_KEY.CHAT_HISTORY_RESET, () => {
  void loadLatest().catch(() => void 0)
})
</script>

<template>
  <div class="size-full flex flex-col justify-end gap-[2%]">
    <div class="min-h-0 flex flex-col justify-end gap-[1.5%] overflow-hidden">
      <div
        v-for="message in bubbles"
        :key="message.id"
        class="max-w-[86%] break-all rounded-[2vw] px-[3%] py-[1.2%] text-[3.4vw] leading-[1.35]"
        :class="message.direction === 'outgoing'
          ? 'self-end bg-[#1677ff] rounded-br-[0.5vw]'
          : 'self-start bg-black/55 rounded-bl-[0.5vw]'"
      >
        {{ bubbleText(message) }}
      </div>
    </div>

    <div
      class="pointer-events-auto flex items-center gap-[2%] rounded-[3vw] bg-black/55 px-[3%] py-[1.5%]"
      @mousedown.stop
    >
      <span
        class="shrink-0 cursor-pointer text-[4.5vw] transition"
        :class="props.recording
          ? 'i-lucide:mic animate-pulse color-[#ff7875]'
          : 'i-lucide:mic color-white/70 hover:color-white'"
        :title="props.pending && !props.recording
          ? $t('pages.main.hints.reRecord')
          : $t('pages.main.hints.voice')"
        @click="handleVoice"
      />

      <textarea
        ref="input"
        v-model="draft"
        class="min-w-0 flex-1 resize-none text-[3.4vw] leading-[1.4] outline-none bg-transparent placeholder:color-white/40"
        :placeholder="hint || $t('pages.chat.placeholders.input')"
        rows="1"
        @keydown="handleKeydown"
      />

      <span
        class="size-[7vw] flex shrink-0 items-center justify-center transition rounded-full"
        :class="canSend ? 'cursor-pointer bg-[#1677ff] hover:bg-[#4096ff]' : 'bg-white/20'"
        :title="$t('pages.main.hints.send')"
        @click="handleSend"
      >
        <span class="i-lucide:arrow-up text-[4vw] color-white" />
      </span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, useTemplateRef } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage } from '@/composables/usePair'

import { MESSAGE_TEXT_LIMIT, RECORDING_LIMIT_SECS } from '@/composables/usePair'
import { usePairChat } from '@/composables/usePairChat'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY } from '@/constants'
import { pairStateKey, usePairStore } from '@/stores/pair'

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

/** 实际显示几条：设置值与浮层上限里更小的那个 */
const shownCount = computed(() => Math.min(bubbleCount.value, OVERLAY_BUBBLE_MAX))

const bubbles = computed(() => messages.value.slice(-shownCount.value))

/**
 * 超出的那几条去哪了：浮层是窄条，只说最新几条，剩下的在聊天窗口里。
 *
 * 不写这句时，浮层看着就是「全部聊天记录」，用户不会想到要去开聊天窗口。
 */
const moreNote = computed(() => {
  return messages.value.length > shownCount.value ? t('pages.chat.hints.moreInChat') : ''
})

const draftBytes = computed(() => new TextEncoder().encode(draft.value).length)
const tooLong = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT)
const canSend = computed(() => {
  return online.value && !sending.value && !tooLong.value && draft.value.trim().length > 0
})

/**
 * 「现在发不出去」的原因（联机状态那句人话）。
 *
 * 录音 / 待确认期间不显示：那两件事有自己的一条提示，挤在一起看不清。
 */
const pairState = computed(() => {
  const key = pairStateKey(pairStore.runtime.connection, pairStore.settings.enabled)

  return key ? t(key) : ''
})

/**
 * 输入框里的 placeholder：只说**这一会儿正在发生什么**（录音 / 录好待确认）。
 *
 * 联机状态与发送失败不写在这儿：它们在下一条状态行上（`blockNote`）——placeholder 一有字
 * 就被挡住，而且两处都写会同屏说两遍。
 */
const hint = computed(() => {
  if (props.recording) {
    return t('pages.main.hints.recording', { seconds: props.recordingSeconds, limit: RECORDING_LIMIT_SECS })
  }

  // R41：录完先不发送，提示去下面那条「试听 / 发送 / 取消」上确认
  if (props.pending) return t('pages.chat.hints.voiceReady')

  return ''
})

/**
 * 输入条下面那一行：只在真的发不出去（或上一次发失败了）时出现。
 *
 * 以前这些都写在 placeholder 上，可发送失败时草稿**不会被清空**，placeholder 被自己
 * 的字挡住——用户只看到发送键变灰、点了没反应，没有任何原因可看。
 */
const blockNote = computed(() => {
  if (props.recording || props.pending) return ''

  return pairState.value || sendError.value
})

/**
 * 输入条下面那一行只写一句：能说的原因（发不出去 / 上次失败）优先，其次是「还有更多消息」。
 *
 * 浮层本来就是窄条，三行字会把气泡挤出上沿（气泡上限 3 条就是按这个高度定的），
 * 所以两件事共用一个位置，不叠两行。
 */
const statusNote = computed(() => blockNote.value || moreNote.value)

/** 附件消息在气泡里只显示一个短标签，正文仍然是 `text` */
function bubbleText(message: ChatMessage) {
  if (message.text) return message.text
  if (message.kind === 'image') return t('pages.main.hints.bubbleImage')
  if (message.kind === 'voice') return t('pages.main.hints.bubbleVoice')

  return t('pages.main.hints.bubbleFile')
}

/**
 * 附件消息（图片 / 语音 / 文件）：浮层里只显示一个 `[语音]` 这样的小标签。
 *
 * 播放语音、看图、另存附件都在独立聊天窗口里，而那个窗口默认是关着的；浮层上又没说
 * 它在哪。所以附件气泡做成可点：点一下把聊天窗口打开，用户就能在那里处理（R44）。
 */
function attachmentMessage(message: ChatMessage) {
  return Boolean(message.attachmentId) && !message.text
}

function openChatWindow() {
  pairStore.settings.chat.visible = true
}

/**
 * 猫咪窗口的根节点在 mousedown 时会 `startDragging()`；拖动一起，那条气泡上的 click 就
 * 收不到了。所以可点的附件气泡要把这一下拦下来（和下面那条输入条同一处理）。
 */
function handleBubbleMouseDown(event: MouseEvent, message: ChatMessage) {
  if (attachmentMessage(message)) event.stopPropagation()
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

/**
 * Enter 发送、Shift+Enter 换行、Esc 退出输入（与聊天窗口同一套键位）。
 *
 * 中文/日文输入法里「确认候选词」也是 Enter，此时 `event.isComposing` 为真，绝不能当成
 * 发送；所以这里自己 `preventDefault`，而不是用 `.prevent` 修饰符。个别 Chromium 版本在
 * 「结束合成的那一次 Enter」上给的是 `isComposing === false` + `keyCode === 229`，再补一条兜底。
 */
function handleKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault()
    inputRef.value?.blur()

    return
  }

  if (event.key !== 'Enter' || event.shiftKey) return
  if (event.isComposing || event.keyCode === 229) return

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
        :class="[
          message.direction === 'outgoing'
            ? 'self-end bg-[#1677ff] rounded-br-[0.5vw]'
            : 'self-start bg-black/55 rounded-bl-[0.5vw]',
          attachmentMessage(message) ? 'cursor-pointer' : '',
        ]"
        :title="attachmentMessage(message) ? $t('pages.main.hints.openInChat') : ''"
        @click="attachmentMessage(message) && openChatWindow()"
        @mousedown="handleBubbleMouseDown($event, message)"
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
          : 'i-lucide:mic color-[#ffffffb2] hover:text-[#fff]'"
        :title="props.pending && !props.recording
          ? $t('pages.main.hints.reRecord')
          : $t('pages.main.hints.voice')"
        @click="handleVoice"
      />

      <textarea
        ref="input"
        v-model="draft"
        class="min-w-0 flex-1 resize-none text-[3.4vw] leading-[1.4] outline-none bg-transparent placeholder:color-[#ffffff66]"
        :placeholder="hint || $t('pages.chat.placeholders.input')"
        rows="1"
        @keydown="handleKeydown"
      />

      <span
        class="size-[7vw] flex shrink-0 items-center justify-center transition rounded-full"
        :class="canSend ? 'cursor-pointer bg-[#1677ff] hover:bg-[#4096ff]' : 'bg-[#ffffff33]'"
        :title="$t('pages.main.hints.send')"
        @click="handleSend"
      >
        <span class="i-lucide:arrow-up text-[4vw] text-[#fff]" />
      </span>
    </div>

    <!-- 发不出去的原因 / 上一次发送失败 / 更早的消息在哪：写在框外，框里一有字 placeholder 就看不见了 -->
    <p
      v-if="statusNote"
      class="truncate px-[3%] text-[2.8vw] color-[#ffffff99]"
      :title="statusNote"
    >
      {{ statusNote }}
    </p>
  </div>
</template>

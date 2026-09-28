<script setup lang="ts">
import { computed, onMounted, ref, useTemplateRef } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage } from '@/composables/usePair'

import { MESSAGE_TEXT_LIMIT, RECORDING_LIMIT_SECS } from '@/composables/usePair'
import { usePairChat } from '@/composables/usePairChat'
import { setChatVisible } from '@/composables/usePairOverlay'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY } from '@/constants'
import { outboundBlockKey, pairStateKey, usePairStore } from '@/stores/pair'

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

/**
 * 现在发不出去的原因（i18n key）；能发时是空串。
 *
 * 两条会合方式各有一条硬理由（判据在 store 里，见 `outboundBlockKey`）：配对码没有服务器
 * 兜底，**公益档**那台服务器只帮忙打洞、不中继。两条都要在按下麦克风之前就挡住——不能让
 * 用户录完一分钟才发现发不出去。
 */
const blockKey = computed(() => {
  return outboundBlockKey({
    tier: pairStore.runtime.tier,
    p2p: pairStore.runtime.p2p,
    manual: pairStore.runtime.manual,
  })
})

const blocked = computed(() => Boolean(blockKey.value))

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
  return (
    online.value
    && !blocked.value
    && !sending.value
    && !tooLong.value
    && draft.value.trim().length > 0
  )
})

/** 配对码模式要单独说一句：它连上之前「对方离线」这种说法会把人指错方向 */
const manualState = computed(() => {
  const manual = pairStore.runtime.manual

  if (!manual || manual.phase === 'connected') return ''

  return t(`pages.preference.pair.manual.phase.${manual.phase}`)
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

  if (manualState.value) return t('pages.chat.hints.manual', { state: manualState.value })

  // 公益档那句要排在「先排队、等连上再发」前面：这一档排队也不会发出去
  if (blockKey.value) return t(blockKey.value)

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
  setChatVisible(true)
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
  if (blocked.value) return

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
  <!--
    R46：浮层里的字号、圆角、间距都用固定像素，不随猫的缩放变大变小（以前是 vw / %，
    跟着窗口宽度走）。浮层那一块的高度仍按猫的比例留（R39），猫缩得很小时放不下的旧气泡
    会从上沿被裁掉，输入条始终贴在最下面。
  -->
  <div class="cat-chat size-full flex flex-col justify-end gap-[7px] px-[7px] pt-[4px] text-[#fff]">
    <div class="min-h-0 flex flex-col justify-end gap-[4px] overflow-hidden">
      <div
        v-for="message in bubbles"
        :key="message.id"
        class="overlay-bubble max-w-[86%] shrink-0 break-all rounded-[14px] px-[11px] py-[6px] text-[12px] leading-[1.45]"
        :class="[
          message.direction === 'outgoing'
            ? 'self-end bg-[#69559d] rounded-br-[4px]'
            : 'self-start bg-[#35303f] rounded-bl-[4px]',
          attachmentMessage(message) ? 'cursor-pointer' : '',
        ]"
        :role="attachmentMessage(message) ? 'button' : undefined"
        :tabindex="attachmentMessage(message) ? 0 : undefined"
        :title="attachmentMessage(message) ? $t('pages.main.hints.openInChat') : ''"
        @click="attachmentMessage(message) && openChatWindow()"
        @keydown.enter.prevent="attachmentMessage(message) && openChatWindow()"
        @keydown.space.prevent="attachmentMessage(message) && openChatWindow()"
        @mousedown="handleBubbleMouseDown($event, message)"
      >
        {{ bubbleText(message) }}
      </div>
    </div>

    <div
      class="overlay-compose pointer-events-auto flex shrink-0 items-center gap-[8px] rounded-[16px] px-[9px] py-[6px]"
      @mousedown.stop
    >
      <button
        :aria-label="props.recording ? $t('pages.main.hints.stopRecording') : $t('pages.main.hints.voice')"
        class="voice-button size-[28px] flex shrink-0 items-center justify-center text-[16px] rounded-full"
        :class="blocked ? 'cursor-not-allowed' : 'cursor-pointer'"
        :disabled="blocked"
        :title="props.pending && !props.recording
          ? $t('pages.main.hints.reRecord')
          : $t('pages.main.hints.voice')"
        type="button"
        @click="handleVoice"
      >
        <span :class="props.recording ? 'i-lucide:square animate-pulse text-[#ffb0bc]' : 'i-lucide:mic text-[#ead5f5]'" />
      </button>

      <textarea
        ref="input"
        v-model="draft"
        :aria-label="$t('pages.chat.placeholders.input')"
        class="min-w-0 flex-1 resize-none text-[12px] leading-[1.4] outline-none bg-transparent placeholder:color-[#ffffff66]"
        :placeholder="hint || $t('pages.chat.placeholders.input')"
        rows="1"
        @keydown="handleKeydown"
      />

      <button
        :aria-label="$t('pages.main.hints.send')"
        class="size-[28px] flex shrink-0 items-center justify-center transition rounded-full"
        :class="canSend ? 'cursor-pointer bg-[#7964bc] hover:bg-[#917bd2]' : 'bg-[#ffffff14]'"
        :disabled="!canSend"
        :title="$t('pages.main.hints.send')"
        type="button"
        @click="handleSend"
      >
        <span class="i-lucide:arrow-up text-[13px] text-[#fff]" />
      </button>
    </div>

    <!--
      发不出去的原因 / 上一次发送失败 / 更早的消息在哪：写在框外，框里一有字 placeholder 就看不见了。
      猫缩得很小时（见下面的 <style>）这一行先让位，保住输入条。
    -->
    <p
      v-if="statusNote"
      class="overlay-status shrink-0 truncate px-[10px] py-[2px] text-[10px] color-[#eee3f5] rounded-full"
      :title="statusNote"
    >
      {{ statusNote }}
    </p>
  </div>
</template>

<style scoped>
.overlay-bubble {
  border: 1px solid #bca6ce50;
  box-shadow: 0 2px 5px #100a2020;
}
.overlay-compose {
  background: #2c2637;
  border: 1px solid #796887;
  box-shadow: 0 3px 10px #130d2229;
}
.overlay-compose:focus-within {
  border-color: #d3b5e6;
}
.voice-button {
  background: #ffffff0c;
}
.voice-button:hover {
  background: #ffffff20;
}
.overlay-status {
  background: #2c2637;
}
.cat-chat button:focus-visible,
.overlay-bubble:focus-visible {
  box-shadow: 0 0 0 2px #eed7fc;
}
.cat-chat button:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
/*
 * R46：浮层里的东西是固定像素，浮层那一块的高度却随猫缩放（约占窗口高的 38%）。
 * 输入条 + 状态行要约 60px，也就是窗口矮于约 160px 时就放不下了——先把状态行藏起来，
 * 输入条（约 42px）还能完整显示到窗口约 110px 高。
 */
@media (max-height: 160px) {
  .overlay-status {
    display: none;
  }
}
</style>

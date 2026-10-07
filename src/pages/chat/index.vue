<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { writeImage, writeText } from '@tauri-apps/plugin-clipboard-manager'
import { save } from '@tauri-apps/plugin-dialog'
import { copyFile, readFile } from '@tauri-apps/plugin-fs'
import { error } from '@tauri-apps/plugin-log'
import { openPath } from '@tauri-apps/plugin-opener'
import { useEventListener, useResizeObserver } from '@vueuse/core'
import { computed, nextTick, onMounted, onUnmounted, ref, useTemplateRef, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage, MessageStatus, TransferProgress } from '@/composables/usePair'

import { useChatVoice } from '@/composables/useChatVoiceBridge'
import { MESSAGE_TEXT_LIMIT, pairSetMaxAttachmentMb, RECORDING_LIMIT_SECS } from '@/composables/usePair'
import { formatMessageTime, usePairChat } from '@/composables/usePairChat'
import { setChatVisible } from '@/composables/usePairOverlay'
import { usePairStatus } from '@/composables/usePairStatus'
import {
  acceptTransfer,
  attachmentTitle,
  canCancel,
  cancelTransfer,
  extensionOf,
  formatFileSize,
  isTransferActive,
  localPathOf,
  needsDecision,
  previewableImage,
  rejectTransfer,
  retryTransfer,
  transferLabelKey,
  usePairTransfer,
} from '@/composables/usePairTransfer'
import { usePairVoicePlayback } from '@/composables/usePairVoicePlayback'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, setAlwaysOnTop, showWindowByLabel } from '@/plugins/window'
import { outboundBlockKey, pairStateKey, usePairStore } from '@/stores/pair'

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
const { messages, hasMore, loading, loadLatest, loadOlder, send, apply } = usePairChat()
const { state: voice, ready: voiceReady, bridgeError, action: voiceAction } = useChatVoice()
const { transferOf, apply: applyTransfer, reset: resetTransfers } = usePairTransfer()
const {
  isFailed: voiceFailed,
  isPlaying: voicePlaying,
  labelOf: voiceLabel,
  percentOf: voicePercent,
  stop: stopVoice,
  toggle: toggleVoice,
} = usePairVoicePlayback()
const listRef = useTemplateRef<HTMLElement>('list')
const contentRef = useTemplateRef<HTMLElement>('content')
const inputRef = useTemplateRef<HTMLTextAreaElement>('input')

const atNewest = ref(true)
const unread = ref(0)
const historyError = ref('')
let prepending = false
const inputMode = ref(false)
const draft = ref('')
const sending = ref(false)
const sendError = ref('')
const copiedId = ref('')
const saving = ref(false)
/** 打开图片预览时的那条消息（§37） */
const previewMessage = ref<ChatMessage>()
/** 一闪而过的提示：复制图片、另存为的结果 */
const notice = ref('')
let copiedTimer: ReturnType<typeof setTimeout> | undefined
let noticeTimer: ReturnType<typeof setTimeout> | undefined

usePairStatus()

/** 输入的内容按 UTF-8 字节算，和 Rust 侧的限制对齐（§31） */
const draftBytes = computed(() => new TextEncoder().encode(draft.value).length)
const tooLong = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT)
const showingLimit = computed(() => draftBytes.value > MESSAGE_TEXT_LIMIT * 0.8)

function resizeComposer() {
  const input = inputRef.value
  if (!input) return
  input.style.height = '36px'
  input.style.height = `${Math.min(112, Math.max(36, input.scrollHeight))}px`
}

watch(draft, resizeComposer, { flush: 'post' })
useEventListener('resize', resizeComposer)
onMounted(() => nextTick(resizeComposer))

/**
 * 现在发不出去的原因（i18n key）；能发时是空串。
 *
 * 两条会合方式各有一条硬理由，都是「发出去只会掉进黑洞，而本地那条已经被标成已发送」：
 * 配对码（没有服务器兜底）与**公益档**（那台服务器只帮忙打洞、不中继）。判据在 store 里
 * （`outboundBlockKey`，有单测），Rust 侧 `outbound_blocked` 用的是同一套。
 */
const blockKey = computed(() => {
  return outboundBlockKey({
    tier: pairStore.runtime.tier,
    p2p: pairStore.runtime.p2p,
    manual: pairStore.runtime.manual,
  })
})

const blocked = computed(() => Boolean(blockKey.value))

/**
 * 挡住时标题那一行说的话：它原本写着「在线 · 随时聊聊」，而这会儿根本发不出去。
 *
 * 这里只放一句短的：完整那句（带「等上面显示『已直连』」）在输入条下面已经有了，
 * 而标题那行是 `truncate` 的，塞不下反而会把有用的后半句截掉。
 */
const blockState = computed(() => {
  if (!blocked.value) return ''

  // 公益档下「打洞失败」与「还在打通」不是一回事：前者要用户动手（换服务器 / 换网络），
  // 所以标题也换一句。判据**从 `blockKey` 读**，不在这里再抄一遍（正文那句在输入条下面
  // 用的也是同一个 key）：抄一份就会出现「标题说打洞失败、正文说配对码没连上」。
  const failed = blockKey.value === 'pages.chat.hints.publicFailed'

  return t(failed ? 'pages.chat.hints.blockedTitlePublicFailed' : 'pages.chat.hints.blockedTitle')
})

/** R42：发送键能不能按——有内容、没超长、不在发送中、没有「发不出去」的硬理由 */
const sendReady = computed(() => {
  return Boolean(draft.value.trim()) && !tooLong.value && !sending.value && !blocked.value
})

/**
 * 联机状态那句人话（标题栏与输入框下面都用它）。
 *
 * R44：以前只分「联机没打开」和「对方离线」，于是「没连上服务器 / 正在连 / 连不上」都长成
 * 「对方离线」——用户会去等对方，而真实原因可能是自己地址填错或服务器没开。
 */
const pairState = computed(() => {
  const key = pairStateKey(pairStore.runtime.connection, pairStore.settings.enabled)

  return key ? t(key) : ''
})

/**
 * 配对码模式要单独说一句。
 *
 * 下面那条「先排队、等连上再发」在配对码这条路上**不成立**：没有服务器兜底，排队就是永远
 * 发不出去（Rust 侧也会直接拒）。所以这里换成实话——等直连建立再发。
 */
const manualState = computed(() => {
  const manual = pairStore.runtime.manual

  if (!manual || manual.phase === 'connected') return ''

  return t(`pages.preference.pair.manual.phase.${manual.phase}`)
})

/** 穿透开着时点不到窗口，提示一句，免得用户以为窗口坏了 */
const footerHint = computed(() => {
  if (pairStore.settings.chat.passThrough && !inputMode.value) {
    // 穿透开着时整窗点不到，唯一入口是快捷键——那句文案里也要把它说了（见 zh-CN / en-US）
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
  if (voice.value.recording) void voiceAction('release')
  // R42：窗口都藏了就别在后台接着放语音
  stopVoice()
  hideWindowByLabel(WINDOW_LABEL.CHAT).catch(reason => error(String(reason)))
}, { immediate: true })

async function scrollToNewest() {
  await nextTick()

  const list = listRef.value

  if (list) list.scrollTop = list.scrollHeight
}

/** Prepend history without moving the message the reader is looking at. */
async function stepOlder() {
  const list = listRef.value
  if (!list || loading.value || !hasMore.value || prepending) return
  historyError.value = ''
  let anchor: HTMLElement | undefined
  let top = 0
  try {
    const count = await loadOlder(() => {
      prepending = true
      atNewest.value = false
      anchor = [...list.querySelectorAll<HTMLElement>('[data-message]')]
        .find(element => element.getBoundingClientRect().bottom > list.getBoundingClientRect().top)
      top = anchor?.getBoundingClientRect().top ?? 0
    })
    await nextTick()
    if (count && anchor?.isConnected) {
      list.scrollTop += anchor.getBoundingClientRect().top - top
    }
  } catch (reason) {
    historyError.value = String(reason)
  } finally {
    prepending = false
  }
}

function handleScroll() {
  const list = listRef.value
  if (!list || prepending) return
  atNewest.value = list.scrollHeight - list.scrollTop - list.clientHeight < 48
  if (atNewest.value) unread.value = 0
  if (list.scrollTop < 64 && !historyError.value) void stepOlder()
}

// Images, wrapping text and a growing composer can change the viewport size.
// Follow those changes only while the reader is already at the latest message.
useResizeObserver([listRef, contentRef], () => {
  if (atNewest.value && !prepending) void scrollToNewest()
})

async function handleSend() {
  // 被挡住的时候连回车也不要发：`sendReady` 只管住了发送键，而 `@keydown.enter` 走的是
  // 这条路。放它过去只会从 Rust 那边弹回一条红字，说的其实是同一件事（而那句是中文）。
  if (blocked.value) return

  if (!draft.value.trim() || tooLong.value || sending.value) return

  sending.value = true
  sendError.value = ''

  try {
    // 连不上时这条消息留在本地队列里（§32），状态显示为「等待发送」
    await send(draft.value)
    draft.value = ''
    atNewest.value = true
    unread.value = 0
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

/** 一闪而过的提示：贴在标题栏下面，两三秒后自己消失 */
function flash(text: string) {
  notice.value = text

  if (noticeTimer) clearTimeout(noticeTimer)

  noticeTimer = setTimeout(() => {
    notice.value = ''
  }, 2500)
}

/** 附件缩略图 / 语音播放用的 asset 地址；还没落到本机时返回空 */
function assetSource(item: ChatMessage) {
  const path = localPathOf(item.attachment)

  return path ? convertFileSrc(path) : void 0
}

/** R42：这条语音能不能播——文件真的落到本机了才算（还在传的没有本地路径） */
function canPlayVoice(item: ChatMessage) {
  return item.kind === 'voice' && Boolean(assetSource(item))
}

/** R42：点播放键。地址可能刚好在这一刻还没有，所以在这里再取一次 */
function toggleVoiceOf(item: ChatMessage) {
  const source = assetSource(item)

  if (!source) return

  void toggleVoice(item.id, source)
}

function openPreview(item: ChatMessage) {
  if (!previewableImage(item)) return

  previewMessage.value = item
}

function closePreview() {
  previewMessage.value = void 0
}

const previewSource = computed(() => {
  const item = previewMessage.value

  return item ? assetSource(item) : void 0
})

function handleAccept(item: ChatMessage) {
  acceptTransfer(item.id).catch(reason => flash(String(reason)))
}

function handleReject(item: ChatMessage) {
  rejectTransfer(item.id).catch(reason => flash(String(reason)))
}

function handleCancel(item: ChatMessage) {
  cancelTransfer(item.id).catch(reason => flash(String(reason)))
}

/** §43：失败的附件只能由发出去的那一方重发 */
function handleRetry(item: ChatMessage) {
  retryTransfer(item.id).catch(reason => flash(String(reason)))
}

/** §42：打开必须由用户明确点，绝不自动执行 */
function handleOpen(item: ChatMessage) {
  const path = localPathOf(item.attachment)

  if (!path) return

  openPath(path).catch(reason => flash(String(reason)))
}

async function handleSaveAs(item: ChatMessage) {
  const path = localPathOf(item.attachment)

  if (!path || saving.value) return

  saving.value = true

  try {
    const target = await save({ defaultPath: attachmentTitle(item.attachment) ?? 'attachment' })

    if (target) {
      await copyFile(path, target)
      closePreview()
    }
  } catch (reason) {
    flash(String(reason))
  } finally {
    saving.value = false
  }
}

/**
 * 交给剪贴板的内容：PNG 直接把路径给 Rust，其它格式先在 WebView 里转成 PNG。
 *
 * tauri 只编译了 `image-png`，`writeImage(路径)` 对 jpg / webp 这些会解码失败，
 * 而收进来的图片多半不是 PNG。转不出来时原样返回，让错误照实报给用户。
 */
async function clipboardImageInput(item: ChatMessage) {
  const path = previewableImage(item)

  if (!path) return void 0

  if (extensionOf(path) === 'png') return path

  const bitmap = await createImageBitmap(new Blob([await readFile(path)]))
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height)
  const context = canvas.getContext('2d')

  if (!context) return path

  context.drawImage(bitmap, 0, 0)
  bitmap.close()

  return new Uint8Array(await (await canvas.convertToBlob({ type: 'image/png' })).arrayBuffer())
}

/** §37：把收到的图片放进剪贴板 */
async function handleCopyImage(item: ChatMessage) {
  try {
    const input = await clipboardImageInput(item)

    if (!input) return

    await writeImage(input)
    flash(t('pages.chat.hints.imageCopied'))
  } catch (reason) {
    flash(String(reason))
  }
}

function handleHide() {
  setChatVisible(false)
}

async function backToNewest() {
  atNewest.value = true
  unread.value = 0

  await scrollToNewest()
}

function handleMouseDown(event: MouseEvent) {
  if ((event.target as HTMLElement).closest('button')) return

  appWindow.startDragging()
}

useTauriListen(LISTEN_KEY.CHAT_INPUT_TOGGLE, toggleInput)

useTauriListen(LISTEN_KEY.CHAT_HISTORY_RESET, () => {
  atNewest.value = true
  unread.value = 0
  historyError.value = ''

  closePreview()
  resetTransfers()
  // R42：这一条可能已经不在列表里了，别留着一个放不完的播放器
  stopVoice()

  void loadLatest().then(scrollToNewest).catch((reason) => {
    error(String(reason))
  })
})

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, ({ payload }) => {
  const isNew = !messages.value.some(message => message.id === payload.id)
  apply(payload)

  if (atNewest.value) void scrollToNewest()
  else if (isNew) unread.value += 1
})

useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_UPDATED, ({ payload }) => {
  apply(payload)
  if (atNewest.value) void scrollToNewest()
})

useTauriListen<TransferProgress>(LISTEN_KEY.PAIR_TRANSFER, ({ payload }) => {
  applyTransfer(payload)
})

useEventListener('keydown', (event) => {
  if (event.key !== 'Escape') return

  if (previewMessage.value) {
    closePreview()

    return
  }

  closeInput()
})

onMounted(async () => {
  // §42：附件上限存在偏好里，Rust 侧重启后是默认值，这里把用户设置补回去
  pairSetMaxAttachmentMb(pairStore.settings.chat.attachmentMaxMb).catch((reason) => {
    error(String(reason))
  })

  try {
    await loadLatest()
    await scrollToNewest()
  } catch (reason) {
    error(String(reason))
  }
})

onUnmounted(() => {
  if (copiedTimer) clearTimeout(copiedTimer)
  if (noticeTimer) clearTimeout(noticeTimer)
})
</script>

<template>
  <!--
    R42：窗口本身还是 `transparent`（圆角靠它才成立），但底色改成**不透明**的实色——
    以前是 `bg-black/45`，桌面上任何东西都会透进来，字很难读。气泡上的半透明白都是叠在
    这一层实色上面的，所以不用逐个改。
  -->
  <div class="chat-shell relative size-screen flex flex-col overflow-hidden rounded-2xl">
    <header
      class="chat-header flex shrink-0 cursor-move items-center gap-3 px-4 py-3"
      @mousedown="handleMouseDown"
    >
      <span
        aria-hidden="true"
        class="chat-avatar"
      ><span class="i-lucide:cat" /></span>
      <!-- 状态点：未启用 / 对方离线 / 在线。必须是没有点击事件的元素，否则会变成拖窗口 -->
      <span
        class="size-1.5 shrink-0 rounded-full"
        :class="!pairStore.settings.enabled
          ? 'bg-[#faad14]'
          : (pairStore.runtime.peerOnline ? 'bg-[#52c41a]' : 'bg-[#9c8991]')"
      />

      <div class="min-w-0 flex-1">
        <div class="truncate text-[13px] font-semibold">
          {{ pairStore.runtime.peerName || $t('pages.chat.labels.peer') }}
        </div>
        <div class="chat-presence mt-0.5 truncate text-[11px]">
          {{ manualState || blockState || pairState || $t('pages.chat.hints.connected') }}
        </div>
      </div>

      <button
        v-if="!atNewest"
        class="i-lucide:chevron-down relative shrink-0 cursor-pointer text-[18px]"
        :title="$t('pages.chat.hints.backToNewest')"
        @click="backToNewest"
      />

      <button
        class="i-lucide:x relative shrink-0 cursor-pointer text-[18px]"
        :title="$t('pages.chat.hints.hide')"
        @click="handleHide"
      />
    </header>

    <p
      v-if="notice"
      class="pointer-events-none absolute inset-x-3 top-9 z-50 break-all rounded-[0.5rem] bg-black/75 px-2 py-1 text-center text-[9px] color-[#ffffffd9]"
    >
      {{ notice }}
    </p>

    <div
      ref="list"
      :aria-label="$t('pages.chat.labels.history')"
      class="chat-list min-h-0 flex-1 overflow-y-auto px-3 py-4"
      tabindex="0"
      @scroll.passive="handleScroll"
    >
      <div
        ref="content"
        class="chat-content min-h-full flex flex-col gap-3"
      >
        <div
          v-if="messages.length && hasMore"
          class="history-control"
        >
          <button
            :disabled="loading"
            @click="stepOlder"
          >
            {{ loading ? $t('pages.chat.hints.loading') : $t('pages.chat.hints.loadOlder') }}
          </button>
        </div>
        <p
          v-else-if="messages.length"
          class="history-control"
        >
          {{ $t('pages.chat.hints.historyStart') }}
        </p>
        <p
          v-if="historyError"
          class="chat-error"
          role="alert"
        >
          {{ historyError }}
        </p>
        <div
          v-if="loading && !messages.length"
          class="flex flex-1 flex-col items-center justify-center gap-2"
        >
          <span class="i-lucide:message-circle animate-pulse text-[22px]" />
          <span class="text-[10px]">{{ $t('pages.chat.hints.loading') }}</span>
        </div>

        <div
          v-else-if="!messages.length"
          class="flex flex-1 flex-col items-center justify-center gap-2"
        >
          <span class="empty-avatar"><span class="i-lucide:messages-square" /></span>
          <span class="mt-2 text-[12px]">{{ $t('pages.chat.hints.empty') }}</span>
        </div>

        <div
          v-for="item in messages"
          :key="item.id"
          class="group message-row flex"
          :class="item.direction === 'outgoing' ? 'justify-end' : 'justify-start'"
          :data-message="item.id"
        >
          <div
            class="chat-bubble max-w-[92%] min-w-0 break-words px-3 py-2.5 text-[14px] leading-[1.6] rounded-2xl"
            :class="item.direction === 'outgoing'
              ? 'bubble-out rounded-br-md'
              : 'bubble-in rounded-bl-md'"
          >
            <template v-if="item.attachment">
              <button
                v-if="previewableImage(item)"
                class="block cursor-pointer"
                :title="$t('pages.chat.hints.preview')"
                @click="openPreview(item)"
              >
                <img
                  alt=""
                  class="max-h-40 max-w-full object-cover rounded-md"
                  :src="assetSource(item)"
                >
              </button>

              <template v-else-if="item.kind === 'voice'">
                <!--
                R42：不再用系统原生 `<audio controls>`（那条「播放 / 0:00 / 下载 / ⋮」），
                换成自绘的一行：播放键 + 进度条 + 时长。播放走 `new Audio()` + asset 协议，
                与猫咪窗口里试听录音同一套（`usePairVoicePlayback`）。
              -->
                <div class="voice-message min-w-0 flex items-center gap-2.5">
                  <button
                    class="size-9 flex shrink-0 items-center justify-center transition rounded-full"
                    :class="canPlayVoice(item) ? 'cursor-pointer bg-[#ffffff26] hover:bg-[#ffffff40]' : 'bg-[#ffffff1a] opacity-40'"
                    :disabled="!canPlayVoice(item)"
                    :title="canPlayVoice(item)
                      ? (voicePlaying(item.id) ? $t('pages.chat.player.pause') : $t('pages.chat.player.play'))
                      : $t('pages.chat.player.waiting')"
                    @click="toggleVoiceOf(item)"
                  >
                    <span
                      class="text-[13px] text-[#fff]"
                      :class="voicePlaying(item.id) ? 'i-lucide:pause' : 'i-lucide:play'"
                    />
                  </button>

                  <div class="min-w-0 flex-1">
                    <div class="progress-track h-1 w-full overflow-hidden rounded-full">
                      <div
                        class="progress-fill h-full transition-[width] duration-150"
                        :style="{ width: `${voicePercent(item.id)}%` }"
                      />
                    </div>

                    <div class="mt-0.5 flex items-center justify-between gap-1 text-[9px] color-[#ffffff99]">
                      <span class="truncate">
                        {{ voiceFailed(item.id)
                          ? $t('pages.chat.player.failed')
                          : (canPlayVoice(item) ? $t('pages.chat.hints.voice') : $t('pages.chat.player.waiting')) }}
                      </span>

                      <span class="shrink-0">{{ voiceLabel(item.id) }}</span>
                    </div>
                  </div>
                </div>
              </template>

              <template v-else>
                <div class="flex items-center gap-1.5">
                  <span class="i-lucide:file shrink-0 text-[14px]" />

                  <span class="min-w-0 truncate">
                    {{ attachmentTitle(item.attachment) || $t('pages.chat.hints.attachment') }}
                  </span>
                </div>
              </template>

              <div
                v-if="item.kind !== 'voice'"
                class="mt-1 flex items-center gap-1 text-[10px] color-[#ffffff99]"
              >
                <span>{{ formatFileSize(item.attachment.size ?? 0) }}</span>
              </div>

              <!-- 传输进度（§38）：还没结束才显示 -->
              <template v-if="transferOf(item.id)">
                <div class="progress-track mt-1 h-1 w-full overflow-hidden rounded-full">
                  <div
                    class="progress-fill h-full"
                    :style="{ width: `${transferOf(item.id)!.percent}%` }"
                  />
                </div>

                <div class="mt-0.5 flex items-center justify-between gap-1 text-[9px] color-[#ffffffb2]">
                  <span>{{ $t(`pages.chat.transfer.${transferLabelKey(transferOf(item.id)!)}`) }}</span>

                  <span v-if="isTransferActive(transferOf(item.id)!.state)">
                    {{ formatFileSize(transferOf(item.id)!.transferred) }}
                    /
                    {{ formatFileSize(transferOf(item.id)!.size) }}
                  </span>
                </div>

                <p
                  v-if="transferOf(item.id)!.message"
                  class="mt-0.5 break-all text-[9px]"
                  :class="transferOf(item.id)!.state === 'failed' ? 'text-[#ff7875]' : 'color-[#ffffff99]'"
                >
                  {{ transferOf(item.id)!.message }}
                </p>

                <p
                  v-if="item.status === 'failed' && item.direction === 'incoming'"
                  class="mt-0.5 text-[9px] color-[#ffffff99]"
                >
                  {{ $t('pages.chat.hints.askPeerResend') }}
                </p>
              </template>

              <!-- 接收方：大文件先问一句（§42） -->
              <div
                v-if="needsDecision(transferOf(item.id))"
                class="mt-1 flex items-center gap-2"
              >
                <button
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleAccept(item)"
                >
                  {{ $t('pages.chat.buttons.accept') }}
                </button>

                <button
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleReject(item)"
                >
                  {{ $t('pages.chat.buttons.reject') }}
                </button>
              </div>

              <div
                v-if="localPathOf(item.attachment) || canCancel(transferOf(item.id)) || (item.status === 'failed' && item.direction === 'outgoing')"
                class="mt-1.5 flex flex-wrap items-center gap-1"
              >
                <button
                  v-if="item.kind !== 'image' && item.kind !== 'voice' && localPathOf(item.attachment)"
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleOpen(item)"
                >
                  {{ $t('pages.chat.buttons.open') }}
                </button>

                <button
                  v-if="localPathOf(item.attachment)"
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleSaveAs(item)"
                >
                  {{ $t('pages.chat.buttons.saveAs') }}
                </button>

                <button
                  v-if="canCancel(transferOf(item.id))"
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleCancel(item)"
                >
                  {{ $t('pages.chat.buttons.cancel') }}
                </button>

                <button
                  v-if="item.status === 'failed' && item.direction === 'outgoing'"
                  class="cursor-pointer bg-[#ffffff1f] px-1.5 py-0.5 text-[10px] rounded-md hover:bg-[#ffffff38]"
                  @click="handleRetry(item)"
                >
                  {{ $t('pages.chat.buttons.retry') }}
                </button>
              </div>
            </template>

            <p
              v-else
              class="message-text whitespace-pre-wrap"
            >
              {{ item.text }}
            </p>

            <div class="bubble-meta mt-1.5 flex flex-wrap items-center justify-end gap-1.5 text-[10px]">
              <button
                v-if="item.text"
                class="relative shrink-0 cursor-pointer text-[12px] opacity-50 transition group-focus-within:opacity-100 group-hover:opacity-100"
                :class="copiedId === item.id ? 'i-lucide:check' : 'i-lucide:copy'"
                :title="$t('pages.chat.hints.copy')"
                @click="handleCopy(item)"
              />

              <time class="message-time">{{ formatMessageTime(item.createdAt) }}</time>

              <span
                v-if="item.direction === 'outgoing'"
                class="shrink-0 text-[10px]"
                :class="[STATUS_ICON[item.status], item.status === 'failed' ? 'text-[#b42348]' : '']"
                :title="statusTitle(item.status)"
              />
            </div>
          </div>
        </div>
      </div>
    </div>

    <button
      v-if="!atNewest"
      class="latest-button"
      @click="backToNewest"
    >
      <span class="i-lucide:arrow-down" />
      {{ unread ? $t('pages.chat.hints.newMessages', { count: unread }) : $t('pages.chat.hints.backToNewest') }}
    </button>

    <div
      class="chat-composer shrink-0 p-3"
    >
      <div
        v-if="voice.recording || voice.pending"
        class="voice-draft"
        role="status"
      >
        <span class="voice-draft-label">
          {{ voice.recording
            ? $t('pages.main.hints.recording', { seconds: voice.seconds, limit: RECORDING_LIMIT_SECS })
            : $t('pages.main.hints.recordingReady', { seconds: voice.pendingSeconds }) }}
        </span>
        <button
          v-if="voice.recording"
          class="voice-action"
          :disabled="voice.busy"
          @click="voiceAction('release')"
        >
          {{ $t('pages.main.hints.stopRecording') }}
        </button>
        <button
          v-else
          class="voice-action"
          :disabled="voice.sending || voice.busy"
          @click="stopVoice(); voiceAction('play')"
        >
          {{ voice.playing ? $t('pages.main.hints.pauseRecording') : $t('pages.main.hints.playRecording') }}
        </button>
        <button
          class="voice-action"
          :disabled="voice.sending || voice.busy"
          @click="voiceAction('cancel')"
        >
          {{ $t('pages.chat.buttons.cancel') }}
        </button>
        <button
          v-if="!voice.recording"
          class="voice-action voice-send"
          :disabled="voice.sending || voice.busy || Boolean(voice.blockReason) || blocked"
          :title="voice.blockReason ? $t(voice.blockReason) : ''"
          @click="voiceAction('send')"
        >
          {{ voice.sending ? $t('pages.main.hints.sendingRecording') : $t('pages.chat.buttons.send') }}
        </button>
      </div>
      <p
        v-if="voice.error || bridgeError"
        class="chat-error"
        role="alert"
      >
        {{ voice.error || bridgeError }}
      </p>
      <p
        v-else-if="voice.skipped"
        class="chat-error"
        role="status"
      >
        {{ $t('pages.main.hints.recordingTooShort') }}
      </p>
      <p
        v-if="voice.pending && voice.blockReason"
        class="chat-error"
      >
        {{ $t(voice.blockReason) }}
      </p>
      <button
        v-if="!voiceReady"
        class="history-control"
        @click="voiceAction('sync')"
      >
        {{ $t('pages.chat.hints.voiceUnavailable') }}
      </button>
      <div class="composer-field">
        <button
          :aria-label="voice.recording ? $t('pages.main.hints.stopRecording') : $t('pages.main.hints.voice')"
          class="composer-voice"
          :disabled="!voiceReady || voice.busy || voice.sending || voice.pending || (!voice.recording && (blocked || Boolean(voice.blockReason)))"
          :title="voice.recording ? $t('pages.main.hints.stopRecording') : voice.blockReason ? $t(voice.blockReason) : $t('pages.main.hints.voice')"
          @click="stopVoice(); voiceAction(voice.recording ? 'release' : 'press')"
        >
          <span :class="voice.recording ? 'i-lucide:square' : 'i-lucide:mic'" />
        </button>
        <textarea
          ref="input"
          v-model="draft"
          :aria-label="$t('pages.chat.placeholders.input')"
          class="composer-input"
          :placeholder="$t('pages.chat.placeholders.input')"
          rows="1"
          :title="$t('pages.chat.hints.inputKeys')"
          @focus="inputMode = true"
          @keydown.enter.exact="handleSendKey"
          @keydown.esc.prevent="closeInput"
        />

        <button
          :aria-label="$t('pages.chat.buttons.send')"
          class="send-button"
          :disabled="!sendReady"
          :title="$t('pages.chat.buttons.send')"
          @click="handleSend"
        >
          <span class="i-lucide:arrow-up text-[13px] text-[#fff]" />
        </button>
      </div>

      <div
        v-if="showingLimit || (pairStore.settings.chat.passThrough && !inputMode)"
        class="composer-hint"
      >
        <span v-if="pairStore.settings.chat.passThrough && !inputMode">{{ footerHint }}</span>

        <span
          v-if="showingLimit"
          :class="tooLong ? 'text-[#ff7875]' : ''"
        >
          {{ $t('pages.chat.hints.textLimit', { bytes: draftBytes }) }}
        </span>
      </div>

      <!--
        联机不正常时说一句人话。聊天窗口离线也能排队（§32），所以这里不是「发不出去」，
        而是「先排队、等连上再发」——以前这件事只写在标题栏里，用户不会往上看。
      -->
      <p
        v-if="manualState"
        class="mt-1 break-all text-[9px] color-[#ffffff99]"
      >
        {{ $t('pages.chat.hints.manual', { state: manualState }) }}
      </p>

      <p
        v-else-if="blockKey"
        class="mt-1 break-all text-[9px] color-[#ffffff99]"
      >
        {{ $t(blockKey) }}
      </p>

      <p
        v-else-if="pairState"
        class="mt-1 break-all text-[9px] color-[#ffffff99]"
      >
        {{ $t('pages.chat.hints.queued', { state: pairState }) }}
      </p>

      <p
        v-if="sendError"
        class="mt-1 break-all text-[9px] text-[#ff7875]"
      >
        {{ sendError }}
      </p>
    </div>

    <!-- 图片预览（§37）：复制图片 / 另存为 -->
    <div
      v-if="previewMessage"
      class="absolute inset-0 z-50 flex flex-col bg-black/95 text-white rounded-2xl"
    >
      <header
        class="flex shrink-0 cursor-move items-center gap-1.5 px-2.5 py-1.5"
        @mousedown="handleMouseDown"
      >
        <span class="min-w-0 flex-1 truncate text-[11px] color-[#ffffff8c]">
          {{ attachmentTitle(previewMessage.attachment) || $t('pages.chat.hints.attachment') }}
        </span>

        <button
          class="i-lucide:x relative shrink-0 cursor-pointer text-[14px] color-[#ffffff99] before:absolute hover:text-[#fff] before:content-empty before:-inset-[0.4em]"
          :title="$t('pages.chat.hints.close')"
          @click="closePreview"
        />
      </header>

      <div class="min-h-0 flex flex-1 items-center justify-center p-2">
        <img
          alt=""
          class="max-h-full max-w-full object-contain"
          :src="previewSource"
        >
      </div>

      <div class="flex shrink-0 items-center justify-center gap-3 px-2.5 pb-2 text-[10px]">
        <button
          class="cursor-pointer underline hover:text-[#fff]"
          @click="handleCopyImage(previewMessage)"
        >
          {{ $t('pages.chat.buttons.copyImage') }}
        </button>

        <button
          class="cursor-pointer underline hover:text-[#fff]"
          :disabled="saving"
          @click="handleSaveAs(previewMessage)"
        >
          {{ $t('pages.chat.buttons.saveAs') }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.chat-shell {
  /* RainyToken: sakura #FFD1DC, pink #FF85A2, accent #E91E63. */
  --chat-muted: #806371;
  background: #fff8fa;
  border: 1px solid #efd8e1;
  color: #392b33;
  font-family: system-ui, sans-serif;
}
.chat-header {
  background: #fff1f5;
  border-bottom: 1px solid #efd8e1;
}
.chat-presence,
.composer-hint,
.bubble-meta {
  color: var(--chat-muted);
}
.chat-avatar,
.empty-avatar {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  background: #ffd1dc;
  color: #9f2854;
  border: 1px solid #f0b2c7;
}
.chat-avatar {
  width: 34px;
  height: 34px;
  border-radius: 12px;
  font-size: 21px;
}
.empty-avatar {
  width: 62px;
  height: 62px;
  border-radius: 23px;
  font-size: 28px;
  transform: rotate(-7deg);
}
.chat-list {
  background: #fff8fa;
  overscroll-behavior: contain;
  /* Native anchoring also handles images loading above the reader after pagination. */
  overflow-anchor: auto;
  scrollbar-gutter: stable;
  scrollbar-width: thin;
  scrollbar-color: #d9a8ba transparent;
}
.message-row {
  flex-shrink: 0;
}
.message-text {
  overflow-wrap: anywhere;
  user-select: text;
}
.message-time {
  font-variant-numeric: tabular-nums;
}
.chat-bubble {
  box-shadow: 0 2px 5px #6d244909;
}
.bubble-out {
  background: #ffd1dc;
  border: 1px solid #f4bace;
  --chat-muted: #795064;
}
.bubble-in {
  background: #ffffff;
  border: 1px solid #ecdfe5;
}
.chat-bubble button {
  color: #9f2854;
}
.chat-bubble button:hover {
  background-color: #e91e6312;
}
.chat-bubble [class*='color-'] {
  color: var(--chat-muted);
}
.chat-bubble [class*='bg-[#ffffff'] {
  background-color: #b8467429;
}
.voice-message button span {
  color: #9f2854;
}
.progress-track {
  background: #b8467429;
}
.progress-fill {
  background: #bd2456;
}
.voice-message {
  width: 174px;
  max-width: 100%;
}
.chat-composer {
  background: #fff1f5;
  border-top: 1px solid #efd8e1;
  max-height: 55%;
  overflow-y: auto;
}
.chat-composer > p {
  color: var(--chat-muted);
  font-size: 11px;
}
.chat-composer > .chat-error,
.chat-error {
  color: #ae244c;
  font-size: 12px;
  overflow-wrap: anywhere;
  margin-bottom: 6px;
}
.composer-field {
  display: flex;
  align-items: flex-end;
  gap: 8px;
  padding: 8px;
  border-radius: 18px;
  border: 1px solid #e6c5d2;
  background: #ffffff;
  box-shadow: 0 2px 8px #9f285408;
  transition:
    border-color 160ms,
    box-shadow 160ms;
}
.composer-input {
  display: block;
  flex: 1;
  min-width: 0;
  height: 36px;
  min-height: 36px;
  max-height: 112px;
  padding: 7px 0;
  margin: 0;
  border: 0;
  outline: none;
  resize: none;
  background: transparent;
  color: #392b33;
  font: inherit;
  font-size: 14px;
  line-height: 22px;
  scrollbar-width: thin;
  scrollbar-color: #d9a8ba transparent;
}
.composer-hint {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin: 8px 4px 0;
  font-size: 11px;
  line-height: 1.5;
}
.composer-field:focus-within {
  border-color: #e91e63;
  box-shadow: 0 0 0 3px #ff85a21c;
}
.composer-field textarea::placeholder {
  color: #927b87;
}
.send-button {
  width: 36px;
  height: 36px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  border-radius: 12px;
  background: #bd2456;
  color: white;
  transition: background 160ms;
}
.chat-shell .send-button:disabled {
  background: #f4e1e9;
  color: #a98b98;
  opacity: 1;
}
.send-button span {
  color: inherit;
  font-size: 17px;
}
.send-button:hover:not(:disabled) {
  background: #a51f4b;
}
.composer-voice {
  width: 36px;
  height: 36px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  color: #b12b59;
  font-size: 19px;
  border-radius: 12px;
  background: #fff1f5;
}
.composer-voice:hover:not(:disabled) {
  background: #fff1f5;
}
.voice-draft {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  padding: 10px;
  margin-bottom: 10px;
  background: #fff;
  border: 1px solid #f0b2c7;
  border-radius: 12px;
  font-size: 12px;
}
.voice-draft-label {
  flex: 1 1 100%;
}
.voice-action {
  padding: 6px 10px;
  min-height: 32px;
  background: #fff1f5;
  color: #9f2854;
  border-radius: 8px;
}
.voice-send {
  background: #bd2456;
  color: white;
  margin-left: auto;
}
.history-control {
  text-align: center;
  color: var(--chat-muted);
  font-size: 11px;
  padding: 2px 0 6px;
}
.history-control button {
  padding: 6px 14px;
  border-radius: 12px;
  background: #ffeaf1;
}
.latest-button {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 7px 12px;
  font-size: 12px;
  background: #ffe4ee;
  color: #9f2854;
  flex-shrink: 0;
  border-top: 1px solid #efd8e1;
}
.chat-shell button:focus-visible {
  outline: 2px solid #bd2456;
  outline-offset: 2px;
  border-radius: 5px;
}
.chat-shell button {
  cursor: pointer;
}
.chat-header button {
  min-width: 28px;
  min-height: 28px;
}
.chat-shell button:disabled {
  cursor: not-allowed;
  opacity: 0.55;
}
@media (max-width: 280px) {
  .chat-header {
    padding: 9px;
    gap: 6px;
  }
  .chat-avatar {
    width: 28px;
    height: 28px;
  }
  .chat-composer {
    padding: 8px;
  }
  .chat-list {
    padding: 10px 8px;
  }
}
@media (max-height: 260px) {
  .chat-header {
    padding: 5px 9px;
    gap: 7px;
  }
  .chat-avatar {
    width: 24px;
    height: 24px;
    border-radius: 8px;
    font-size: 17px;
  }
  .chat-presence {
    display: none;
  }
  .chat-composer {
    padding: 5px;
    max-height: 50%;
    overflow-y: auto;
  }
  .composer-field {
    padding: 4px 8px;
  }
  .chat-list {
    padding: 7px;
    gap: 7px;
  }
}
</style>

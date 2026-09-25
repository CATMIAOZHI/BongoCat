<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { writeImage, writeText } from '@tauri-apps/plugin-clipboard-manager'
import { open, save } from '@tauri-apps/plugin-dialog'
import { copyFile, readFile, writeFile } from '@tauri-apps/plugin-fs'
import { error } from '@tauri-apps/plugin-log'
import { openPath } from '@tauri-apps/plugin-opener'
import { useEventListener } from '@vueuse/core'
import { computed, nextTick, onMounted, ref, useTemplateRef, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ChatMessage, MessageStatus, SendAttachmentOptions, TransferProgress } from '@/composables/usePair'

import { MESSAGE_TEXT_LIMIT, pairSendAttachment, pairSetMaxAttachmentMb, pairTransferPaths } from '@/composables/usePair'
import { formatClock, usePairChat, visibleWindow } from '@/composables/usePairChat'
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
  pastedImageName,
  previewableImage,
  rejectTransfer,
  retryTransfer,
  transferKindOfFile,
  transferLabelKey,
  usePairTransfer,
} from '@/composables/usePairTransfer'
import { usePairVoicePlayback } from '@/composables/usePairVoicePlayback'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, setAlwaysOnTop, showWindowByLabel } from '@/plugins/window'
import { pairStateKey, usePairStore } from '@/stores/pair'
import { join } from '@/utils/path'

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
const inputRef = useTemplateRef<HTMLTextAreaElement>('input')

/** 从最新一条往回数的条数：0 就是「看最新的一屏」 */
const offset = ref(0)
const inputMode = ref(false)
const draft = ref('')
const sending = ref(false)
const sendError = ref('')
const copiedId = ref('')
const attaching = ref(false)
const attachmentError = ref('')
const saving = ref(false)
/** 打开图片预览时的那条消息（§37） */
const previewMessage = ref<ChatMessage>()
/** 一闪而过的提示：复制图片、另存为的结果 */
const notice = ref('')
let copiedTimer: ReturnType<typeof setTimeout> | undefined
let noticeTimer: ReturnType<typeof setTimeout> | undefined

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

/** R42：发送键能不能按——有内容、没超长、不在发送中 */
const sendReady = computed(() => {
  return Boolean(draft.value.trim()) && !tooLong.value && !sending.value
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

const peerTitle = computed(() => {
  return pairState.value || pairStore.runtime.peerName || t('pages.chat.labels.peer')
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
  // R42：窗口都藏了就别在后台接着放语音
  stopVoice()
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

/** 发一个附件：落库、显示、滚到最新（§38） */
async function sendAttachment(options: SendAttachmentOptions) {
  if (attaching.value) return

  attaching.value = true
  attachmentError.value = ''

  try {
    apply(await pairSendAttachment(options))

    offset.value = 0

    await scrollToNewest()
  } catch (reason) {
    attachmentError.value = String(reason)
  } finally {
    attaching.value = false
  }
}

/**
 * §37：粘贴进来的图片先落成临时文件，再交给附件管线。
 *
 * `stage` 会让 Rust 把临时文件收进附件缓存，所以临时目录里不会留下副本。
 * 图片不当文本粘贴，否则输入框里会出现一串二进制乱码。
 */
async function handlePaste(event: ClipboardEvent) {
  const data = event.clipboardData

  if (!data) return

  const item = Array.from(data.items).find(entry => entry.type.startsWith('image/'))
  const blob = item?.getAsFile()

  if (!blob) return

  event.preventDefault()

  // 上一张还在算校验值时不要再写一个临时文件，否则它会留在临时目录里没人管
  if (attaching.value) {
    flash(t('pages.chat.hints.attachmentBusy'))

    return
  }

  try {
    const paths = await pairTransferPaths()
    const path = join(paths.tmp, pastedImageName(blob.type, Date.now()))

    await writeFile(path, new Uint8Array(await blob.arrayBuffer()))
    await sendAttachment({ path, kind: 'image', mime: blob.type, stage: true })
  } catch (reason) {
    attachmentError.value = String(reason)
  }
}

/** 从对话框里挑一个文件发送 */
async function pickAttachment() {
  if (attaching.value) return

  try {
    const selected = await open({ directory: false, multiple: false })

    if (typeof selected !== 'string') return

    const name = selected.split(/[\\/]/).pop() ?? selected

    await sendAttachment({ path: selected, kind: transferKindOfFile(name) })
  } catch (reason) {
    attachmentError.value = String(reason)
  }
}

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

  closePreview()
  resetTransfers()
  // R42：这一条可能已经不在列表里了，别留着一个放不完的播放器
  stopVoice()

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
</script>

<template>
  <!--
    R42：窗口本身还是 `transparent`（圆角靠它才成立），但底色改成**不透明**的实色——
    以前是 `bg-black/45`，桌面上任何东西都会透进来，字很难读。气泡上的半透明白都是叠在
    这一层实色上面的，所以不用逐个改。
  -->
  <div class="relative size-screen flex flex-col overflow-hidden bg-[#191c22] text-[#fff] ring-1 ring-[#ffffff1a] rounded-2xl">
    <header
      class="flex shrink-0 cursor-move items-center gap-2 border-b border-[#ffffff14] bg-[#ffffff0d] px-2.5 py-2"
      @mousedown="handleMouseDown"
    >
      <!-- 状态点：未启用 / 对方离线 / 在线。必须是没有点击事件的元素，否则会变成拖窗口 -->
      <span
        class="size-1.5 shrink-0 rounded-full"
        :class="!pairStore.settings.enabled
          ? 'bg-[#faad14]'
          : (pairStore.runtime.peerOnline ? 'bg-[#52c41a]' : 'bg-[#ffffff4c]')"
      />

      <span class="min-w-0 flex-1 truncate text-[11px] color-[#ffffffcc] font-medium">
        {{ peerTitle }}
      </span>

      <button
        v-if="!atNewest"
        class="i-lucide:chevron-down relative shrink-0 cursor-pointer text-[14px] color-[#ffffff8c] before:absolute hover:text-[#fff] before:content-empty before:-inset-[0.4em]"
        :title="$t('pages.chat.hints.backToNewest')"
        @click="backToNewest"
      />

      <button
        class="i-lucide:x relative shrink-0 cursor-pointer text-[14px] color-[#ffffff8c] before:absolute hover:text-[#fff] before:content-empty before:-inset-[0.4em]"
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
      class="min-h-0 flex flex-1 flex-col gap-1 overflow-y-auto px-2 py-2 [&::-webkit-scrollbar]:w-1.5 [&::-webkit-scrollbar-thumb]:bg-[#ffffff26] [&::-webkit-scrollbar-track]:bg-transparent [&::-webkit-scrollbar-thumb]:rounded-full [&::-webkit-scrollbar-thumb]:hover:bg-[#ffffff40]"
      @wheel="handleWheel"
    >
      <div
        v-if="loading && !messages.length"
        class="flex flex-1 flex-col items-center justify-center gap-2 color-[#ffffff66]"
      >
        <span class="i-lucide:message-circle animate-pulse text-[22px]" />
        <span class="text-[10px]">{{ $t('pages.chat.hints.loading') }}</span>
      </div>

      <div
        v-else-if="!messages.length"
        class="flex flex-1 flex-col items-center justify-center gap-2 color-[#ffffff59]"
      >
        <span class="i-lucide:message-circle text-[22px]" />
        <span class="text-[10px]">{{ $t('pages.chat.hints.empty') }}</span>
      </div>

      <div
        v-for="item in visibleMessages"
        :key="item.id"
        class="group flex"
        :class="item.direction === 'outgoing' ? 'justify-end' : 'justify-start'"
      >
        <div
          class="max-w-[86%] break-words px-2.5 py-1.5 text-[12px] leading-[1.4] rounded-2xl shadow-sm"
          :class="item.direction === 'outgoing'
            ? 'bg-[#1677ff] rounded-br-md'
            : 'bg-[#ffffff1a] ring-1 ring-[#ffffff1a] rounded-bl-md'"
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
              <div class="min-w-32 flex items-center gap-2">
                <button
                  class="size-7 flex shrink-0 items-center justify-center transition rounded-full"
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
                  <div class="h-1 w-full overflow-hidden bg-[#ffffff33] rounded-full">
                    <div
                      class="h-full bg-[#ffffffe6] transition-[width] duration-150"
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

            <div class="mt-1 flex items-center gap-1 text-[9px] color-[#ffffff99]">
              <span>{{ formatFileSize(item.attachment.size ?? 0) }}</span>
            </div>

            <!-- 传输进度（§38）：还没结束才显示 -->
            <template v-if="transferOf(item.id)">
              <div class="mt-1 h-1 w-full overflow-hidden bg-[#ffffff33] rounded-full">
                <div
                  class="h-full bg-[#ffffffcc]"
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
                v-if="item.kind !== 'image' && localPathOf(item.attachment)"
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
            class="whitespace-pre-wrap break-all"
          >
            {{ item.text }}
          </p>

          <div class="mt-1 flex items-center justify-end gap-1 text-[9px] color-[#ffffff73]">
            <button
              v-if="item.text"
              class="relative shrink-0 cursor-pointer text-[10px] opacity-0 transition before:absolute hover:text-[#fff] group-hover:opacity-100 before:content-empty before:-inset-[0.4em]"
              :class="copiedId === item.id ? 'i-lucide:check' : 'i-lucide:copy'"
              :title="$t('pages.chat.hints.copy')"
              @click="handleCopy(item)"
            />

            <span>{{ formatClock(item.createdAt) }}</span>

            <span
              v-if="item.direction === 'outgoing'"
              class="shrink-0 text-[10px]"
              :class="[STATUS_ICON[item.status], item.status === 'failed' ? 'text-[#ff7875]' : 'color-[#ffffffb2]']"
              :title="statusTitle(item.status)"
            />
          </div>
        </div>
      </div>
    </div>

    <div
      v-if="inputMode"
      class="shrink-0 border-t border-[#ffffff14] p-1.5"
    >
      <!-- R42：输入框改成「一个盒子 + 圆形发送键」，和猫咪窗口浮层的输入条同款 -->
      <div class="flex items-end gap-1.5 bg-[#ffffff14] px-2 py-1.5 rounded-xl">
        <button
          class="i-lucide:paperclip relative mb-1 shrink-0 cursor-pointer text-[13px] color-[#ffffff8c] before:absolute hover:text-[#fff] before:content-empty before:-inset-[0.4em]"
          :title="$t('pages.chat.hints.pickAttachment')"
          @click="pickAttachment"
        />

        <textarea
          ref="input"
          v-model="draft"
          class="max-h-24 min-h-9 w-full resize-none py-1 text-[12px] leading-[1.4] outline-none bg-transparent placeholder:color-[#ffffff59]"
          :placeholder="$t('pages.chat.placeholders.input')"
          @keydown.enter.exact="handleSendKey"
          @keydown.esc.prevent="closeInput"
          @paste="handlePaste"
        />

        <button
          class="mb-0.5 size-7 flex shrink-0 items-center justify-center transition rounded-full"
          :class="sendReady ? 'cursor-pointer bg-[#1677ff] hover:bg-[#4096ff]' : 'bg-[#ffffff26]'"
          :disabled="!sendReady"
          :title="$t('pages.chat.buttons.send')"
          @click="handleSend"
        >
          <span class="i-lucide:arrow-up text-[13px] text-[#fff]" />
        </button>
      </div>

      <div class="mt-1 flex items-center justify-between gap-2 px-0.5 text-[9px] color-[#ffffff59]">
        <span class="truncate">{{ $t('pages.chat.hints.inputKeys') }}</span>

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
        v-if="pairState"
        class="mt-1 break-all text-[9px] color-[#ffffff99]"
      >
        {{ $t('pages.chat.hints.queued', { state: pairState }) }}
      </p>

      <p
        v-if="attaching"
        class="mt-1 text-[9px] color-[#ffffff99]"
      >
        {{ $t('pages.chat.hints.sendingAttachment') }}
      </p>

      <p
        v-if="attachmentError"
        class="mt-1 break-all text-[9px] text-[#ff7875]"
      >
        {{ attachmentError }}
      </p>

      <p
        v-if="sendError"
        class="mt-1 break-all text-[9px] text-[#ff7875]"
      >
        {{ sendError }}
      </p>
    </div>

    <div
      v-else
      class="shrink-0 px-2.5 pb-2"
    >
      <button
        class="cursor-pointer text-[9px] color-[#ffffff59] hover:color-[#ffffffb2]"
        @click="openInput"
      >
        {{ footerHint }}
      </button>
    </div>

    <!-- 图片预览（§37）：复制图片 / 另存为 -->
    <div
      v-if="previewMessage"
      class="absolute inset-0 z-50 flex flex-col bg-black/95 rounded-2xl"
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

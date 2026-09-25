<script setup lang="ts">
import type { MotionInfo } from 'easy-live2d'

import { convertFileSrc } from '@tauri-apps/api/core'
import { PhysicalSize } from '@tauri-apps/api/dpi'
import { Menu, PredefinedMenuItem } from '@tauri-apps/api/menu'
import { sep } from '@tauri-apps/api/path'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { exists, readDir } from '@tauri-apps/plugin-fs'
import { useDebounceFn, useEventListener } from '@vueuse/core'
import { round } from 'es-toolkit'
import { nth } from 'es-toolkit/compat'
import { storeToRefs } from 'pinia'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'

import ChatOverlay from '@/components/chat-overlay/index.vue'
import { useAppMenu } from '@/composables/useAppMenu'
import { useDevice } from '@/composables/useDevice'
import { useGamepad } from '@/composables/useGamepad'
import { useKeyStateShortcut } from '@/composables/useKeyStateShortcut'
import { useModel } from '@/composables/useModel'
import { RECORDING_LIMIT_SECS } from '@/composables/usePair'
import { usePairVoiceRecorder } from '@/composables/usePairVoice'
import { useTauriListen } from '@/composables/useTauriListen'
import { CHAT_OVERLAY_RATIO, LISTEN_KEY } from '@/constants'
import { hideWindow, setAlwaysOnTop, setTaskbarVisibility, showWindow } from '@/plugins/window'
import { useCatStore } from '@/stores/cat'
import { useGeneralStore } from '@/stores/general.ts'
import { useModelStore } from '@/stores/model'
import { recordingBlockReasonKey, usePairStore } from '@/stores/pair'
import { useShortcutStore } from '@/stores/shortcut'
import { isImage } from '@/utils/is'
import live2d from '@/utils/live2d'
import { join } from '@/utils/path'
import { isWindows } from '@/utils/platform'
import { clearObject } from '@/utils/shared'

const { startListening } = useDevice()
const appWindow = getCurrentWebviewWindow()
const { modelSize, handleLoad, handleDestroy, handleKeyChange } = useModel()
const catStore = useCatStore()
const { getBaseMenu, getExitMenu } = useAppMenu()
const modelStore = useModelStore()
const generalStore = useGeneralStore()
const pairStore = usePairStore()
const shortcutStore = useShortcutStore()
const { pushToTalk } = storeToRefs(shortcutStore)
const {
  recording,
  seconds: recordingSeconds,
  pending: recordingPending,
  pendingSeconds: recordingPendingSeconds,
  playing: recordingPlaying,
  sending: recordingSending,
  error: recordingError,
  skipped: recordingSkipped,
  discarded: recordingDiscarded,
  press: pressToTalk,
  release: releaseToTalk,
  send: sendRecording,
  play: playRecording,
  cancel: cancelRecording,
} = usePairVoiceRecorder()
const resizing = ref(false)
const backgroundImagePath = ref<string>()
const { stickActive } = useGamepad()

/**
 * R39：猫咪窗口上那条聊天浮层只在**开启双人联机**时出现。
 *
 * 它占窗口顶上一块（高度按 `CHAT_OVERLAY_RATIO` 相对模型高度算），猫咪本体贴底不动，
 * 所以窗口总高 = 模型高 × (1 + 比例)。没开联机时窗口尺寸与过去完全一样。
 */
const overlayVisible = computed(() => pairStore.settings.enabled)

/** 猫咪本体占窗口的百分比：浮层出现时把上面那块让出来 */
const modelAreaPercent = computed(() => {
  const ratio = overlayVisible.value ? CHAT_OVERLAY_RATIO : 0

  return 100 / (1 + ratio)
})

const overlayAreaPercent = computed(() => 100 - modelAreaPercent.value)

/** 录音提示、待确认的语音、失败原因与「太短没录上」共用一块位置 */
const showVoiceOverlay = computed(() => {
  return recording.value
    || Boolean(recordingPending.value)
    || Boolean(recordingError.value)
    || recordingSkipped.value
    || recordingDiscarded.value
    || peerConnectedNotice.value
})

/**
 * R44：连上对方给一句正面反馈。
 *
 * 以前「连上了」只在偏好页的一个小徽标里，猫咪窗口上没有任何迹象——用户填完三个值、
 * 点完「立即连接」，看不到成功，只能去偏好页确认。这里只在**状态真的翻到在线**时提示。
 */
const peerConnectedNotice = ref(false)
let peerNoticeTimer: ReturnType<typeof setTimeout> | undefined

watch(() => pairStore.runtime.peerOnline, (online) => {
  if (!online) return

  peerConnectedNotice.value = true

  if (peerNoticeTimer) clearTimeout(peerNoticeTimer)

  peerNoticeTimer = setTimeout(() => {
    peerConnectedNotice.value = false
  }, 4000)
})

/** 窗口关掉时别留一个还在跑的计时器（`usePairVoice` 里的提示计时器同样是这么收的） */
onUnmounted(() => {
  if (peerNoticeTimer) clearTimeout(peerNoticeTimer)
})

/**
 * R41：待确认的语音能不能发。
 *
 * 对方不在线时发送一定会失败，而 Rust 侧失败路径会把临时 wav 删掉——那等于白录一段。
 * 所以离线时按钮只显示、不生效，免得用户点一下就没了。
 */
const canSendRecording = computed(() => {
  return pairStore.settings.enabled && pairStore.runtime.peerOnline && !recordingSending.value
})

/**
 * 待确认的语音发不出去的原因（i18n key）；能发时是空串。
 *
 * R44：以前只分「联机没打开」和「对方不在线」，于是「正在连接服务器」「还没连上服务器」
 * 「连不上服务器」三种情况也都写成「对方不在线，等对方回来再发」——用户会一直等对方，
 * 而真实原因可能是自己地址填错或服务器没开。这里按连接状态细分。
 *
 * 判定本身在 store 里（`recordingBlockReasonKey`，有单测）：`connected` 这个状态只说明
 * 自己连上了服务器，对方在不在线由 `peerOnline` 单独给（Rust 侧 `connected` 与
 * `connected-peer-offline` 是两个状态）。只看连接状态的话，「录好了、对方也在线」
 * 这个正常状态会被判成「对方不在线」。
 */
const recordingBlockReason = computed(() => {
  return recordingBlockReasonKey({
    enabled: pairStore.settings.enabled,
    connection: pairStore.runtime.connection,
    peerOnline: pairStore.runtime.peerOnline,
    sending: recordingSending.value,
  })
})

/** R41：离线时点「发送」直接不发起（理由同上，别把录音弄丢） */
function handleSendRecording() {
  if (!canSendRecording.value) return

  void sendRecording()
}

/**
 * §45 的按住说话。
 *
 * 注册放在猫咪窗口：它是唯一一直活着、用户也一直看得见的窗口，录音提示与
 * 「取消」都挂在这里，按下与松开就都在同一个窗口里处理（不用跨窗口发事件）。
 */
useKeyStateShortcut(pushToTalk, (pressed) => {
  pressed ? pressToTalk() : releaseToTalk()
})

onMounted(startListening)

onUnmounted(handleDestroy)

/**
 * 目标窗口尺寸（物理像素）：模型尺寸 × 缩放，开着联机时再给聊天浮层留一条（R39）。
 *
 * 窗口的比例从此由**模型 + 浮层**决定，不再是「模型比例」：`useModel().handleResize()`
 * 那条「比例不对就把窗口按模型比例摆正」的纠正在这里用不了，会把浮层那一条挤掉，
 * 所以猫咪窗口自己算尺寸。
 */
function targetWindowSize(scale = catStore.window.scale) {
  if (!modelSize.value) return

  const { width, height } = modelSize.value
  const overlayHeight = overlayVisible.value ? height * CHAT_OVERLAY_RATIO : 0
  const factor = scale / 100

  return {
    width: Math.round(width * factor),
    height: Math.round((height + overlayHeight) * factor),
  }
}

const debouncedResize = useDebounceFn(async () => {
  try {
    if (!modelSize.value) return

    /*
     * R43：窗口尺寸变了，模型必须重新贴合一次。
     *
     * R39 把这块改成「自己算尺寸」时漏掉了这一步——`useModel().handleResize()` 里那句
     * `live2d.resizeModel()` 是 pixi 的 `resizeTo: window` 之外唯一按新窗口重算缩放与
     * 居中的地方（pixi 只改画布尺寸，不会动模型），少了它之后拖窗口猫就不再重绘：
     * 猫停在旧缩放上、背景与按键贴图相对猫错位。
     */
    live2d.resizeModel(modelSize.value)

    // 窗口被拉大/拉小时改的是「缩放」，比例始终由模型 (+浮层) 决定。
    // 反推缩放要用**物理宽度**：`targetWindowSize()` 也是按物理像素设的，用 CSS 像素
    // （`innerWidth`）在非 100% 系统缩放下会每次都算小一档，窗口会一跳一跳地越缩越小。
    const size = await appWindow.size()
    const nextScale = Math.max(10, Math.min(500, round((size.width / modelSize.value.width) * 100)))

    if (nextScale !== catStore.window.scale) {
      catStore.window.scale = nextScale

      return
    }

    const target = targetWindowSize()

    if (target && (size.width !== target.width || size.height !== target.height)) {
      await appWindow.setSize(new PhysicalSize(target))
    }
  } finally {
    resizing.value = false
  }
}, 100)

useEventListener('resize', () => {
  resizing.value = true

  debouncedResize()
})

watch(() => modelStore.currentModel, async (model) => {
  if (!model) return

  // R39：换模型时把聊天浮层那一条也算进窗口高度
  await handleLoad(overlayVisible.value ? CHAT_OVERLAY_RATIO : 0)

  const path = join(model.path, 'resources', 'background.png')

  const existed = await exists(path)

  backgroundImagePath.value = existed ? convertFileSrc(path) : void 0

  clearObject([modelStore.supportKeys, modelStore.pressedKeys])

  const resourcePath = join(model.path, 'resources')
  const groups = ['left-keys', 'right-keys']

  for await (const groupName of groups) {
    const groupDir = join(resourcePath, groupName)
    const files = await readDir(groupDir).catch(() => [])
    const imageFiles = files.filter(file => isImage(file.name))

    for (const file of imageFiles) {
      const fileName = file.name.split('.')[0]

      modelStore.supportKeys[fileName] = join(groupDir, file.name)
    }
  }

  modelStore.modelReady = true
}, { deep: true, immediate: true })

watch([() => catStore.window.scale, modelSize, overlayVisible], async () => {
  const target = targetWindowSize()

  if (!target) return

  await appWindow.setSize(new PhysicalSize(target))
}, { immediate: true })

watch([modelStore.pressedKeys, stickActive], ([keys, stickActive]) => {
  const dirs = Object.values(keys).map((path) => {
    return nth(path.split(sep()), -2)!
  })

  const hasLeft = dirs.some(dir => dir.startsWith('left'))
  const hasRight = dirs.some(dir => dir.startsWith('right'))

  handleKeyChange(true, stickActive.left || hasLeft)
  handleKeyChange(false, stickActive.right || hasRight)
}, { deep: true })

watch(() => catStore.window.visible, async (value) => {
  value ? showWindow() : hideWindow()
})

watch(() => catStore.window.passThrough, (value) => {
  appWindow.setIgnoreCursorEvents(value)
}, { immediate: true })

watch(() => catStore.window.alwaysOnTop, setAlwaysOnTop, { immediate: true })

watch(() => generalStore.app.taskbarVisible, setTaskbarVisibility, { immediate: true })

watch(() => catStore.model.motionSound, live2d.setMotionSoundEnabled, { immediate: true })

watch(() => catStore.model.maxFPS, live2d.setMaxFPS, { immediate: true })

useTauriListen<MotionInfo>(LISTEN_KEY.START_MOTION, ({ payload }) => {
  live2d.startMotion(payload)
})

useTauriListen<number>(LISTEN_KEY.SET_EXPRESSION, ({ payload }) => {
  live2d.setExpression(payload)
})

function handleMouseDown() {
  appWindow.startDragging()
}

async function handleContextmenu(event: MouseEvent) {
  event.preventDefault()

  if (event.shiftKey) return

  const menu = await Menu.new({
    items: [
      ...await getBaseMenu(),
      await PredefinedMenuItem.new({ item: 'Separator' }),
      ...await getExitMenu(),
    ],
  })

  // Temporarily disable always-on-top on Windows so the context menu is not covered
  if (isWindows && catStore.window.alwaysOnTop) {
    setAlwaysOnTop(false)
  }

  await menu.popup()

  // Restore always-on-top after the menu is closed
  if (!isWindows || !catStore.window.alwaysOnTop) return

  setAlwaysOnTop(true)
}

function handleMouseMove(event: MouseEvent) {
  const { buttons, shiftKey, movementX, movementY } = event

  if (buttons !== 2 || !shiftKey) return

  const delta = (movementX + movementY) * 0.5
  const nextScale = Math.max(10, Math.min(catStore.window.scale + delta, 500))

  catStore.window.scale = round(nextScale)
}
</script>

<template>
  <div
    class="relative size-screen overflow-hidden"
    :style="{
      opacity: catStore.window.opacity / 100,
      borderRadius: `${catStore.window.radius}%`,
    }"
    @contextmenu="handleContextmenu"
    @mousedown="handleMouseDown"
    @mousemove="handleMouseMove"
  >
    <!--
      猫咪本体：贴底、按模型比例占一块。镜像只作用在这一层——
      放在根节点上会把 R39 的聊天浮层连文字一起镜像过去。
    -->
    <div
      class="absolute inset-x-0 bottom-0 overflow-hidden children:(absolute size-full)"
      :class="{ '-scale-x-100': catStore.model.mirror }"
      :style="{ height: `${modelAreaPercent}%` }"
    >
      <img
        v-if="backgroundImagePath"
        class="object-cover"
        :src="backgroundImagePath"
      >

      <canvas id="live2dCanvas" />

      <img
        v-for="path in modelStore.pressedKeys"
        :key="path"
        class="object-cover"
        :src="convertFileSrc(path)"
      >
    </div>

    <!-- R39：聊天浮层。只在开启双人联机时占位置，猫咪本体不会被它遮住 -->
    <div
      v-if="overlayVisible"
      class="absolute inset-x-0 top-0"
      :style="{ height: `${overlayAreaPercent}%` }"
    >
      <ChatOverlay
        :pending="Boolean(recordingPending)"
        :recording="recording"
        :recording-seconds="recordingSeconds"
        @voice-start="pressToTalk"
        @voice-stop="releaseToTalk"
      />
    </div>

    <div
      v-show="resizing || !modelStore.modelReady"
      class="absolute inset-0 flex items-center justify-center bg-black"
    >
      <span class="text-center text-[10vw] text-[#fff]">
        {{ resizing ? $t('pages.main.hints.redrawing') : $t('pages.main.hints.switching') }}
      </span>
    </div>

    <div
      v-show="showVoiceOverlay"
      class="absolute inset-x-0 bottom-0 flex flex-col items-center gap-1 pb-[4%]"
    >
      <div
        v-if="recording && !overlayVisible"
        class="pointer-events-auto max-w-full flex items-center gap-1.5 bg-black/70 px-3 py-1 text-[3.5vw] text-[#fff] rounded-full"
        @mousedown.stop
      >
        <span class="i-lucide:mic size-[1.1em] shrink-0 animate-pulse text-[#ff7875]" />

        <span class="min-w-0 break-all text-center">
          {{ $t('pages.main.hints.recording', { seconds: recordingSeconds, limit: RECORDING_LIMIT_SECS }) }}
        </span>

        <!--
          图标保持 1.2em（视觉上不变），但用一层透明的 ::before 把可点范围扩到约 2.2em：
          这个胶囊可以很窄，1.2em 在 300px 宽的窗口上只有 13px，而「取消」点了不可逆
          （Rust 会把临时 wav 删掉）。不能改用 padding 放大——`i-lucide:*` 是用 CSS mask
          画的（`mask-size: 100% 100%`），padding 会把那层 mask 一起放大，图形跟着变大。
        -->
        <span
          class="i-lucide:circle-x relative size-[1.2em] shrink-0 cursor-pointer before:absolute hover:text-[#ff7875] before:content-empty before:-inset-[0.5em]"
          :title="$t('pages.main.hints.cancelRecording')"
          @click="cancelRecording"
        />
      </div>

      <!--
        R41：录完先不发送，先给一次试听与确认（试听 / 发送 / 取消）。
        重录时要等麦克风真的开（最多 5 秒）才丢掉上一份草稿，所以这段时间 `pending` 还在，
        而 `recording` 已经亮了——两个胶囊会同时挂在窗口上。用 `!recording` 让「正在录」优先，
        免得出现「上面写着录音中、下面还挂着待确认」这种自相矛盾的画面。
      -->
      <div
        v-if="recordingPending && !recording"
        class="pointer-events-auto max-w-full flex items-center gap-1.5 bg-black/70 px-3 py-1 text-[3.5vw] text-[#fff] rounded-full"
        @mousedown.stop
      >
        <span
          class="relative size-[1.2em] shrink-0 cursor-pointer before:absolute before:content-empty before:-inset-[0.5em]"
          :class="recordingPlaying ? 'i-lucide:pause' : 'i-lucide:play'"
          :title="recordingPlaying ? $t('pages.main.hints.pauseRecording') : $t('pages.main.hints.playRecording')"
          @click="playRecording"
        />

        <span class="min-w-0 break-all text-center">
          {{ recordingSending
            ? $t('pages.main.hints.sendingRecording')
            : recordingBlockReason
              ? $t(recordingBlockReason)
              : $t('pages.main.hints.recordingReady', { seconds: recordingPendingSeconds }) }}
        </span>

        <span
          class="i-lucide:send relative size-[1.2em] shrink-0 before:absolute before:content-empty before:-inset-[0.5em]"
          :class="canSendRecording
            ? 'cursor-pointer hover:text-[#4096ff]'
            : 'color-[#ffffff59]'"
          :title="recordingSending
            ? $t('pages.main.hints.sendingRecording')
            : recordingBlockReason
              ? $t(recordingBlockReason)
              : $t('pages.main.hints.sendRecording')"
          @click="handleSendRecording"
        />

        <span
          class="i-lucide:circle-x relative size-[1.2em] shrink-0 cursor-pointer before:absolute hover:text-[#ff7875] before:content-empty before:-inset-[0.5em]"
          :title="$t('pages.main.hints.cancelRecording')"
          @click="cancelRecording"
        />
      </div>

      <div
        v-if="recordingError"
        class="max-w-full break-all bg-[#d4380d]/85 px-3 py-1 text-center text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ recordingError }}
      </div>

      <div
        v-if="recordingSkipped"
        class="max-w-full break-all bg-black/70 px-3 py-1 text-center text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ $t('pages.main.hints.recordingTooShort') }}
      </div>

      <!-- R44：重录会丢掉上一段，明说一句，别让它无声消失 -->
      <div
        v-if="recordingDiscarded"
        class="max-w-full break-all bg-black/70 px-3 py-1 text-center text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ $t('pages.main.hints.recordingDiscarded') }}
      </div>

      <!-- R44：连上对方时的正面反馈（默认关掉对方猫时，这是猫咪窗口上唯一的「成功了」） -->
      <div
        v-if="peerConnectedNotice"
        class="max-w-full break-all bg-black/70 px-3 py-1 text-center text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ $t('pages.main.hints.peerConnected') }}
      </div>
    </div>
  </div>
</template>

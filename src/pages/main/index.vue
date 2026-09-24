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
import { usePairStore } from '@/stores/pair'
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
  error: recordingError,
  skipped: recordingSkipped,
  press: pressToTalk,
  release: releaseToTalk,
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

/** 录音提示、失败原因与「太短没发出去」共用一块位置 */
const showVoiceOverlay = computed(() => {
  return recording.value || Boolean(recordingError.value) || recordingSkipped.value
})

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
  resizing.value = false

  if (!modelSize.value) return

  // 窗口被拉大/拉小时改的是「缩放」，比例始终由模型 (+浮层) 决定
  const nextScale = Math.max(10, Math.min(500, round((innerWidth / modelSize.value.width) * 100)))

  if (nextScale !== catStore.window.scale) {
    catStore.window.scale = nextScale

    return
  }

  const target = targetWindowSize()

  if (!target || (innerWidth === target.width && innerHeight === target.height)) return

  await appWindow.setSize(new PhysicalSize(target))
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
        class="pointer-events-auto flex items-center gap-1.5 bg-black/70 px-3 py-1 text-[3.5vw] text-[#fff] rounded-full"
        @mousedown.stop
      >
        <span class="i-lucide:mic size-[1.1em] animate-pulse text-[#ff7875]" />

        <span>
          {{ $t('pages.main.hints.recording', { seconds: recordingSeconds, limit: RECORDING_LIMIT_SECS }) }}
        </span>

        <span
          class="i-lucide:circle-x size-[1.2em] cursor-pointer hover:text-[#ff7875]"
          :title="$t('pages.main.hints.cancelRecording')"
          @click="cancelRecording"
        />
      </div>

      <div
        v-else-if="recordingError"
        class="bg-[#d4380d]/85 px-3 py-1 text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ recordingError }}
      </div>

      <div
        v-else-if="recordingSkipped"
        class="bg-black/70 px-3 py-1 text-[3.5vw] text-[#fff] rounded-full"
      >
        {{ $t('pages.main.hints.recordingTooShort') }}
      </div>
    </div>
  </div>
</template>

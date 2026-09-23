<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { PhysicalSize } from '@tauri-apps/api/dpi'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { exists } from '@tauri-apps/plugin-fs'
import { error } from '@tauri-apps/plugin-log'
import { useDebounceFn, useEventListener } from '@vueuse/core'
import { computed, onMounted, onUnmounted, ref, useTemplateRef, watch } from 'vue'

import type { ModelSize } from '@/composables/useModel'
import type { ChatMessage } from '@/composables/usePair'
import type { PetSnapshot } from '@/composables/usePairActivity'

import { useModel } from '@/composables/useModel'
import { defaultSnapshot, sanitizeSnapshot } from '@/composables/usePairActivity'
import { playPairMessageSound } from '@/composables/usePairMessageSound'
import { usePairStatus } from '@/composables/usePairStatus'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, setAlwaysOnTop, showWindowByLabel } from '@/plugins/window'
import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import live2d from '@/utils/live2d'
import { join } from '@/utils/path'

/**
 * 对方猫咪窗口（§21 - §23）。
 *
 * 它只做四件事：渲染远端模型、把网络快照映射到模型参数、显示对方的暂离牌与输入统计、
 * 跟随自己的窗口设置。
 *
 * R11 的约束在这里最要紧：**不能复用会写共享 store 的加载路径**。
 * `useModel().handleLoad()` 会写 `modelStore.currentMotions` / `currentExpressions` /
 * `shortcuts` 与 `catStore.window.scale`，而这些会经 tauri-pinia 同步回猫咪窗口，
 * 把本机的模型配置改坏。所以这里只调用 `live2d.load()`，尺寸也只存在本窗口的局部 ref。
 * 同理，这里只接收网络快照，不监听任何本机输入事件。
 */

/** §23 的无事件恢复：不同状态各自有 TTL，超过就释放，避免对端「一直按着」 */
const TYPING_TTL_MS = 800
const CLICK_TTL_MS = 500
const SNAPSHOT_TTL_MS = 1500
/** 检查 TTL 的频率 */
const DECAY_INTERVAL_MS = 200

const appWindow = getCurrentWebviewWindow()
const pairStore = usePairStore()
const modelStore = useModelStore()
const { handleMouseRatio, handleKeyChange, handleMouseChange } = useModel()

usePairStatus()

const modelSize = ref<ModelSize>()
const modelReady = ref(false)
const backgroundImagePath = ref<string>()
const remote = ref<PetSnapshot>(defaultSnapshot())
/** §28：对方回来时的系统提示，只闪一下，不进入聊天记录 */
const notice = ref('')
const modelRef = useTemplateRef<HTMLElement>('model')
const flashRef = useTemplateRef<HTMLElement>('flash')
let receivedAt = 0
let decayTimer: ReturnType<typeof setInterval> | undefined
let noticeTimer: ReturnType<typeof setTimeout> | undefined
let soundFailed = false

/** 默认和本机用同一个模型（§22），也可以在偏好页里单独指定 */
function remoteModel() {
  const { modelId } = pairStore.settings.remoteCat

  if (modelId) {
    const matched = modelStore.models.find(model => model.id === modelId)

    if (matched) return matched
  }

  return modelStore.currentModel
}

const isOnline = () => pairStore.runtime.peerOnline

const remoteStats = computed(() => {
  const stats = pairStore.runtime.remoteStats

  return stats?.share ? stats : void 0
})

async function applySize() {
  if (!modelSize.value) return

  const scale = pairStore.settings.remoteCat.scale / 100

  await appWindow.setSize(new PhysicalSize({
    width: Math.round(modelSize.value.width * scale),
    height: Math.round(modelSize.value.height * scale),
  }))

  live2d.resizeModel(modelSize.value)
}

/** `setSize` 之后浏览器才会派发 resize，这里再按真实尺寸适配一次 */
const debouncedResize = useDebounceFn(() => {
  if (modelSize.value) live2d.resizeModel(modelSize.value)
}, 100)

useEventListener('resize', debouncedResize)

async function loadModel() {
  const model = remoteModel()

  if (!model) return

  modelReady.value = false

  try {
    const loaded = await live2d.load(model.path)

    modelSize.value = { width: loaded.width, height: loaded.height }

    const background = join(model.path, 'resources', 'background.png')

    backgroundImagePath.value = await exists(background)
      ? convertFileSrc(background)
      : void 0

    await applySize()
  } catch (reason) {
    error(String(reason))
  } finally {
    modelReady.value = true
  }
}

/**
 * 把远端快照写进模型参数。
 *
 * 只认「对方的左右手 + 鼠标比例」，永远不会读本机的 `modelStore.pressedKeys`
 * （那是本机按键，见 R11）。
 */
function applyRemoteSnapshot() {
  const online = isOnline()
  const elapsed = Date.now() - receivedAt
  const snapshot = online ? remote.value : defaultSnapshot()
  // §23：三个 TTL 各自负责一项，超过就回到「没在动」的样子
  const handsFresh = online && elapsed <= TYPING_TTL_MS
  const clicksFresh = online && elapsed <= CLICK_TTL_MS
  // 对方完全没有活动（1.5 秒内一个包都没有）时，鼠标比例也回到中位
  const settled = online && elapsed <= SNAPSHOT_TTL_MS
  const pointer = settled ? snapshot.pointer : defaultSnapshot().pointer

  handleKeyChange(true, handsFresh && snapshot.keyboard.leftHand)
  handleKeyChange(false, handsFresh && snapshot.keyboard.rightHand)
  handleMouseChange('Left', clicksFresh && snapshot.pointer.leftDown)
  handleMouseChange('Right', clicksFresh && snapshot.pointer.rightDown)
  handleMouseRatio(pointer.x, pointer.y)
}

useTauriListen<PetSnapshot>(LISTEN_KEY.PAIR_PET_STATE, ({ payload }) => {
  // 收到就对端是「刚从网络解密出来」的值；这里再跑一遍量化只是防御，不是信任
  remote.value = sanitizeSnapshot(payload)
  receivedAt = Date.now()
})

onMounted(async () => {
  await loadModel()

  applyRemoteSnapshot()

  decayTimer = setInterval(applyRemoteSnapshot, DECAY_INTERVAL_MS)

  if (pairStore.settings.remoteCat.visible) {
    await showWindowByLabel(WINDOW_LABEL.REMOTE_CAT)
  }
})

onUnmounted(() => {
  if (decayTimer) clearInterval(decayTimer)
  if (noticeTimer) clearTimeout(noticeTimer)

  live2d.destroy()
})

watch(() => pairStore.runtime.remotePresence, (value, previous) => {
  if (!pairStore.settings.away.sendSystemNotice) return
  if (value !== 'active' || previous !== 'away') return

  notice.value = 'back'

  if (noticeTimer) clearTimeout(noticeTimer)

  noticeTimer = setTimeout(() => {
    notice.value = ''
  }, 4000)
})

watch(() => pairStore.settings.remoteCat.visible, (visible) => {
  if (visible) {
    void showWindowByLabel(WINDOW_LABEL.REMOTE_CAT).catch(reason => error(String(reason)))
  } else {
    void hideWindowByLabel(WINDOW_LABEL.REMOTE_CAT).catch(reason => error(String(reason)))
  }
})

watch(() => pairStore.settings.remoteCat.modelId, loadModel)

// 没有单独指定模型时跟随本机模型（本机换模型会经 tauri-pinia 同步过来）
watch(() => modelStore.currentModel, () => {
  if (!pairStore.settings.remoteCat.modelId) void loadModel()
})

watch(() => pairStore.settings.remoteCat.scale, applySize)

watch(() => pairStore.settings.remoteCat.alwaysOnTop, setAlwaysOnTop, { immediate: true })

watch(() => pairStore.settings.remoteCat.passThrough, (value) => {
  appWindow.setIgnoreCursorEvents(value)
}, { immediate: true })

/**
 * §46 的 Q 弹：给猫咪容器加一段短 CSS 动画，绝不移动原生窗口坐标。
 *
 * 动画跑完类名仍然挂着，所以每次都要先摘掉、强制一次重排、再挂回去，
 * 连续来消息时动画才会一条一条地播（改父子状态的方式做不到重播）。
 */
function playBounce() {
  const element = modelRef.value

  if (!element) return

  element.classList.remove('pair-bounce')
  element.getBoundingClientRect()
  element.classList.add('pair-bounce')
}

/** §46 的粉色闪光：叠在猫咪上的短 overlay，同样不动模型文件 */
function playFlash() {
  const element = flashRef.value

  if (!element) return

  element.classList.remove('pair-flash')
  element.getBoundingClientRect()
  element.classList.add('pair-flash')
}

/** §46 的提示音：音量 0 等价静音，播放被系统策略拦下时只记一次日志 */
function playNotificationSound() {
  const { notificationSound, notificationVolume } = pairStore.settings.chat

  if (!notificationSound) return

  playPairMessageSound(notificationVolume).catch((reason) => {
    if (soundFailed) return

    soundFailed = true
    error(`提示音播放失败（可能被自动播放策略拦下）: ${String(reason)}`)
  })
}

function notifyChatMessage() {
  playBounce()
  playFlash()
  playNotificationSound()
}

// §47：聊天窗口负责把消息加进列表，通知动画只在这里做
useTauriListen<ChatMessage>(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, notifyChatMessage)

function handleMouseDown() {
  appWindow.startDragging()
}
</script>

<template>
  <div
    class="relative size-screen overflow-hidden"
    :style="{ opacity: pairStore.settings.remoteCat.opacity / 100 }"
    @mousedown="handleMouseDown"
  >
    <div
      ref="model"
      class="absolute size-full transition-opacity"
      :class="{ 'opacity-50': !isOnline() }"
    >
      <img
        v-if="backgroundImagePath"
        class="absolute size-full object-cover"
        :src="backgroundImagePath"
      >

      <canvas id="live2dCanvas" />
    </div>

    <div
      ref="flash"
      class="pointer-events-none absolute inset-0 bg-[#ff7ac6] opacity-0"
    />

    <div
      v-if="pairStore.runtime.remotePresence === 'away' && isOnline()"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="max-w-full break-all bg-black/55 px-2.5 py-1 text-[11px] text-white rounded-lg">
        {{ pairStore.runtime.remotePresenceMessage || $t('pages.remoteCat.hints.away') }}
      </div>
    </div>

    <div
      v-else-if="notice"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="bg-black/50 px-2.5 py-1 text-[11px] text-white rounded-lg">
        {{ $t('pages.remoteCat.hints.peerBack') }}
      </div>
    </div>

    <div
      v-if="pairStore.settings.remoteCat.showStats && isOnline() && remoteStats"
      class="absolute inset-x-0 bottom-0 flex justify-center pb-1.5"
    >
      <div class="bg-black/45 px-2 py-0.5 text-[10px] color-white/85 rounded-md">
        {{ $t('pages.remoteCat.hints.today', {
          keyboard: remoteStats.todayKeyboard,
          mouse: remoteStats.todayMouse,
        }) }}
        ·
        {{ $t('pages.remoteCat.hints.total', {
          keyboard: remoteStats.totalKeyboard,
          mouse: remoteStats.totalMouse,
        }) }}
      </div>
    </div>

    <div
      v-if="isOnline() && pairStore.runtime.peerName"
      class="absolute inset-x-0 bottom-0 flex justify-center pb-0.5"
    >
      <span class="text-[9px] color-white/50">{{ pairStore.runtime.peerName }}</span>
    </div>

    <div
      v-show="!modelReady"
      class="absolute size-full flex items-center justify-center bg-black"
    >
      <span class="text-center text-[10vw] text-white">
        {{ $t('pages.main.hints.switching') }}
      </span>
    </div>

    <div
      v-if="!pairStore.settings.enabled"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="bg-black/55 px-2 py-1 text-[11px] text-white rounded-lg">
        {{ $t('pages.remoteCat.hints.disabled') }}
      </div>
    </div>

    <div
      v-else-if="!isOnline()"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="bg-black/55 px-2 py-1 text-[11px] text-white rounded-lg">
        {{ $t('pages.remoteCat.hints.offline') }}
      </div>
    </div>
  </div>
</template>

<style scoped>
@keyframes pair-cat-bounce {
  0% {
    transform: translateY(0) scale(1);
  }

  25% {
    transform: translateY(-5%) scale(1.04);
  }

  60% {
    transform: translateY(2%) scale(0.98);
  }

  100% {
    transform: translateY(0) scale(1);
  }
}

.pair-bounce {
  transform-origin: bottom center;
  animation: pair-cat-bounce 0.6s ease-out;
}

@keyframes pair-cat-flash {
  0% {
    opacity: 0;
  }

  20% {
    opacity: 1;
  }

  100% {
    opacity: 0;
  }
}

.pair-flash {
  animation: pair-cat-flash 0.42s ease-out;
}
</style>

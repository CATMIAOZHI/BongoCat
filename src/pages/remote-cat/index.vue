<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { PhysicalSize } from '@tauri-apps/api/dpi'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { exists, readDir } from '@tauri-apps/plugin-fs'
import { error } from '@tauri-apps/plugin-log'
import { useDebounceFn, useEventListener } from '@vueuse/core'
import { round } from 'es-toolkit'
import { computed, onMounted, onUnmounted, ref, useTemplateRef, watch } from 'vue'

import type { ModelSize } from '@/composables/useModel'
import type { ChatMessage } from '@/composables/usePair'
import type { PetSnapshot } from '@/composables/usePairActivity'
import type { Model } from '@/stores/model'

import { getSupportedKey, useModel } from '@/composables/useModel'
import { defaultSnapshot, sanitizeSnapshot } from '@/composables/usePairActivity'
import { playPairMessageSound } from '@/composables/usePairMessageSound'
import { usePairStatus } from '@/composables/usePairStatus'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, setAlwaysOnTop, showWindowByLabel } from '@/plugins/window'
import { useModelStore } from '@/stores/model'
import { pairStateKey, usePairStore } from '@/stores/pair'
import { isImage } from '@/utils/is'
import live2d from '@/utils/live2d'
import { join } from '@/utils/path'
import { clearObject } from '@/utils/shared'

/**
 * 对方猫咪窗口（§21 - §23）。
 *
 * 它做五件事：渲染远端模型、把网络快照映射到模型参数（含 R37 的按键贴图）、
 * 显示对方的暂离牌与输入统计、跟随自己的窗口设置。
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
/**
 * 指针插值的时间常数（§6 / R23 的「远端本地插值」）。
 *
 * 快照是**绝对值 + 量化**的（步长 0.02），60Hz 下目标值仍在跳，所以插值仍然必要：
 * 它把「一跳一跳」变成「跟手」。固定的 100ms 会在 60Hz 下稳定引入 100ms 迟滞，而 §10
 * 要的正是「明显更跟手」，所以 tau 取观测到的快照间隔的 1.5 倍，夹在这个区间里：
 * 60Hz 下约 25ms，3Hz 下封顶 150ms。
 */
const TAU_MIN_MS = 25
const TAU_MAX_MS = 150
/** 观测间隔的夹取范围，防止第一次收到包时算出离谱的值 */
const GAP_MIN_MS = 16
const GAP_MAX_MS = 1000
/** 指针变化小于它就跳过重绘（量化步长是 0.02，这个阈值远小于它） */
const POINTER_EPSILON = 0.001

const appWindow = getCurrentWebviewWindow()
const pairStore = usePairStore()
const modelStore = useModelStore()
const { handleMouseRatio, handleKeyChange, handleMouseChange, handlePress, handleRelease } = useModel()

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
/** 最近一次相邻快照的间隔，用来自适应插值的时间常数 */
let snapshotGapMs = 0
/** 插值中的指针位置（`remote` 是最新目标，这里是渲染中的值） */
let renderedX = 0.5
let renderedY = 0.5
let lastFrameAt = 0
let frameHandle: number | undefined
/** 上一次真正写进模型的参数，用来跳过没有任何变化的帧 */
let appliedKey: string | undefined
let appliedX = Number.NaN
let appliedY = Number.NaN
let noticeTimer: ReturnType<typeof setTimeout> | undefined
let soundFailed = false
/**
 * R37：已经按进本窗口模型的远端键名（归一化之后的）。
 *
 * 只在本窗口自己的 `modelStore.pressedKeys` / `supportKeys` 上动手——这两个字段在
 * `stores/model.ts` 里被排除在跨窗口同步之外（`tauri.filterKeys`），所以不会把本机
 * 猫咪窗口的按键贴图改坏（R11）。
 */
const appliedRemoteKeys = new Set<string>()

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

/**
 * R44：联机状态对应的 i18n key（按连接状态细分，不再把所有情况都说成「对方离线」）。
 * 正常时是空串，模板用 `$t(stateNote)` 取文案。
 */
const stateNote = computed(() => pairStateKey(pairStore.runtime.connection, pairStore.settings.enabled))

const remoteStats = computed(() => {
  const stats = pairStore.runtime.remoteStats

  return stats?.share ? stats : void 0
})

/**
 * 目标窗口尺寸（物理像素）：模型尺寸 × 缩放。
 *
 * 对方猫窗口上没有聊天浮层（R39 那条只在自己猫上），所以不用留额外高度。
 */
function targetRemoteSize(scale = pairStore.settings.remoteCat.scale) {
  if (!modelSize.value) return

  const factor = scale / 100

  return {
    width: Math.round(modelSize.value.width * factor),
    height: Math.round(modelSize.value.height * factor),
  }
}

async function applySize() {
  if (!modelSize.value) return

  const target = targetRemoteSize()

  if (!target) return

  await appWindow.setSize(new PhysicalSize(target))

  live2d.resizeModel(modelSize.value)
}

/** 拖动 / 摆正时盖一层黑罩，避免露出贴图与猫错位的那一帧（与自己猫一致） */
const resizing = ref(false)

/**
 * R43：和猫咪窗口同一套——拖窗口边缘改的是「缩放百分比」，比例永远由模型决定。
 *
 * 以前这里只调 `live2d.resizeModel()`：窗口被自由拉伸、比例不写回，下一次重算尺寸
 * （改设置里的尺寸、换模型、重启）都会跳回去；而且背景与键贴图是 `object-cover` 铺满
 * 窗口的，猫却是等比居中的，拖完必然错位。三段式照抄 `pages/main/index.vue`：
 * 先按新窗口贴合模型，再用窗口宽度反推缩放并写回设置，由上面的 watcher 把窗口摆正。
 */
const debouncedResize = useDebounceFn(async () => {
  try {
    if (!modelSize.value) return

    live2d.resizeModel(modelSize.value)

    // 反推缩放用**物理宽度**，与自己猫（`pages/main/index.vue`）和 `targetRemoteSize()`
    // 的口径一致：用 CSS 像素在非 100% 系统缩放下会每次都算小一档、窗口越缩越小。
    const size = await appWindow.size()
    const nextScale = Math.max(10, Math.min(500, round((size.width / modelSize.value.width) * 100)))

    if (nextScale !== pairStore.settings.remoteCat.scale) {
      pairStore.settings.remoteCat.scale = nextScale

      return
    }

    const target = targetRemoteSize()

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

async function loadModel() {
  const model = remoteModel()

  if (!model) return

  modelReady.value = false

  try {
    const loaded = await live2d.load(model.path)

    modelSize.value = { width: loaded.width, height: loaded.height }
    // 新模型是默认参数：作废「跳过没变化的帧」的记录，否则参数要等下一次变化才补上
    resetApplied()
    await loadSupportKeys(model)
    // 换模型之后贴在旧模型上的键贴图必须立刻摘掉
    appliedRemoteKeys.clear()

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

/** 换模型之后新模型是默认参数，之前「跳过没变化的帧」的记录必须作废 */
function resetApplied() {
  appliedKey = void 0
  appliedX = Number.NaN
  appliedY = Number.NaN
}

/**
 * R37：把**对方模型**的按键贴图扫进本窗口的 `supportKeys`。
 *
 * 猫咪窗口那份扫描写在 `pages/main/index.vue` 里，而这里加载的是「对方猫咪」自己选的
 * 模型（可能与本机不同），所以必须单独扫一次。这两个字段不跨窗口同步，各窗口一份。
 */
async function loadSupportKeys(model: Model) {
  clearObject([modelStore.supportKeys, modelStore.pressedKeys])

  const resourcePath = join(model.path, 'resources')

  for (const groupName of ['left-keys', 'right-keys']) {
    const groupDir = join(resourcePath, groupName)
    const files = await readDir(groupDir).catch(() => [])

    for (const file of files) {
      if (!isImage(file.name)) continue

      modelStore.supportKeys[file.name.split('.')[0]] = join(groupDir, file.name)
    }
  }
}

/**
 * R37：把对方按着的键名变成贴图。只做差量，不重建。
 *
 * 先按**本窗口模型**归一化再比：`F5` 与 `F6` 在模型里都会折叠成 `Fn`，直接按原始
 * 键名释放会把仍然按着的那个键一起放开。
 */
function applyRemoteKeys(names: string[]) {
  const next = new Set(names.map(name => getSupportedKey(modelStore.supportKeys, name)))

  for (const key of appliedRemoteKeys) {
    if (next.has(key)) continue

    handleRelease(key)
  }

  for (const key of next) {
    if (appliedRemoteKeys.has(key)) continue

    handlePress(key)
  }

  appliedRemoteKeys.clear()

  for (const key of next) appliedRemoteKeys.add(key)
}

/** 自适应时间常数：1.5 倍观测间隔，夹在 [TAU_MIN_MS, TAU_MAX_MS] */
function interpolateTau() {
  return Math.min(TAU_MAX_MS, Math.max(TAU_MIN_MS, snapshotGapMs * 1.5))
}

function applyHands(left: boolean, right: boolean, leftDown: boolean, rightDown: boolean) {
  const key = `${left}|${right}|${leftDown}|${rightDown}`

  if (key === appliedKey) return

  appliedKey = key

  handleKeyChange(true, left)
  handleKeyChange(false, right)
  handleMouseChange('Left', leftDown)
  handleMouseChange('Right', rightDown)
}

function applyPointer() {
  if (
    Math.abs(renderedX - appliedX) < POINTER_EPSILON
    && Math.abs(renderedY - appliedY) < POINTER_EPSILON
  ) {
    return
  }

  appliedX = renderedX
  appliedY = renderedY

  handleMouseRatio(renderedX, renderedY)
}

/**
 * 把远端快照写进模型参数（§6 / R23 的远端插值）。
 *
 * 只认「对方的左右手 + 鼠标比例」，永远不会读本机的 `modelStore.pressedKeys`
 * （那是本机按键，见 R11）。
 *
 * 每帧都跑，但只有真的变了才写模型参数：指针位置按时间指数趋近最新快照，TTL 判定与
 * 离线回中位照旧。窗口隐藏时浏览器会节流 rAF，恢复可见后的第一帧 `dt` 很大 →
 * `1 - exp(-dt / tau) ≈ 1` 直接吸附到目标，这就是它自愈的方式（不是 bug）。
 */
function renderRemoteSnapshot() {
  const now = Date.now()
  const dt = lastFrameAt === 0 ? Number.POSITIVE_INFINITY : Math.max(0, now - lastFrameAt)

  lastFrameAt = now

  const online = isOnline()
  const elapsed = now - receivedAt
  const snapshot = online ? remote.value : defaultSnapshot()
  // §23：两个 TTL 各自负责一项，超过就回到「没在动」的样子
  const handsFresh = online && elapsed <= TYPING_TTL_MS
  const clicksFresh = online && elapsed <= CLICK_TTL_MS
  // 鼠标停在哪就留在哪（本机猫也是这样）；只有离线时才回到中位。以前 1.5 秒没包就回中位，
  // 对方鼠标一停爪子就跳回屏幕中间，看起来像鼠标没同步
  const pointer = online ? snapshot.pointer : defaultSnapshot().pointer

  if (online) {
    const alpha = 1 - Math.exp(-dt / interpolateTau())

    renderedX += (pointer.x - renderedX) * alpha
    renderedY += (pointer.y - renderedY) * alpha
  } else {
    // 离线：直接回中位，不做插值（本窗口的第一帧靠上面 dt=∞ → alpha=1 吸附）
    renderedX = pointer.x
    renderedY = pointer.y
  }

  applyHands(
    handsFresh && snapshot.keyboard.leftHand,
    handsFresh && snapshot.keyboard.rightHand,
    clicksFresh && snapshot.pointer.leftDown,
    clicksFresh && snapshot.pointer.rightDown,
  )
  // R37：键名跟着 TYPING_TTL 一起过期——对方把猫放下时，贴图也要放下
  applyRemoteKeys(handsFresh ? snapshot.keyboard.keys : [])
  applyPointer()
}

function frameLoop() {
  frameHandle = requestAnimationFrame(frameLoop)

  renderRemoteSnapshot()
}

useTauriListen<PetSnapshot>(LISTEN_KEY.PAIR_PET_STATE, ({ payload }) => {
  // 收到就对端是「刚从网络解密出来」的值；这里再跑一遍量化只是防御，不是信任
  const now = Date.now()

  if (receivedAt > 0) {
    snapshotGapMs = Math.min(GAP_MAX_MS, Math.max(GAP_MIN_MS, now - receivedAt))
  }

  remote.value = sanitizeSnapshot(payload)
  receivedAt = now
})

onMounted(async () => {
  await loadModel()

  renderRemoteSnapshot()

  frameLoop()

  if (pairStore.settings.remoteCat.visible) {
    await showWindowByLabel(WINDOW_LABEL.REMOTE_CAT)
  }
})

onUnmounted(() => {
  if (frameHandle !== undefined) cancelAnimationFrame(frameHandle)
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
    class="group relative size-screen overflow-hidden"
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

      <!-- R37：对方按着的键贴图，和猫咪窗口同一套渲染方式 -->
      <img
        v-for="path in modelStore.pressedKeys"
        :key="path"
        class="absolute size-full object-cover"
        :src="convertFileSrc(path)"
      >
    </div>

    <div
      ref="flash"
      class="pointer-events-none absolute inset-0 bg-[#ff7ac6] opacity-0"
    />

    <div
      v-if="pairStore.runtime.remotePresence === 'away' && isOnline()"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="max-w-full break-all rounded-[0.5rem] bg-black/55 px-2.5 py-1 text-[11px] text-[#fff]">
        {{ pairStore.runtime.remotePresenceMessage || $t('pages.remoteCat.hints.away') }}
      </div>
    </div>

    <div
      v-else-if="notice"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="rounded-[0.5rem] bg-black/50 px-2.5 py-1 text-[11px] text-[#fff]">
        {{ $t('pages.remoteCat.hints.peerBack') }}
      </div>
    </div>

    <!--
      统计平时藏起来，鼠标挪到对方猫上才显示，免得一直挡住猫。
      开着「窗口穿透」时鼠标悬停不会生效，只能一直显示，否则就永远看不到了。
    -->
    <div
      v-if="pairStore.settings.remoteCat.showStats && isOnline() && remoteStats"
      class="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center pb-1.5 transition-opacity"
      :class="{ 'opacity-0 group-hover:opacity-100': !pairStore.settings.remoteCat.passThrough }"
    >
      <div class="bg-black/45 px-2 py-0.5 text-[10px] color-[#ffffffd9] rounded-md">
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
      class="absolute inset-x-0 bottom-0 flex justify-center pb-0.5 transition-opacity"
      :class="{ 'group-hover:opacity-0': remoteStats && pairStore.settings.remoteCat.showStats && !pairStore.settings.remoteCat.passThrough }"
    >
      <span class="text-[9px] color-[#ffffff80]">{{ pairStore.runtime.peerName }}</span>
    </div>

    <div
      v-show="resizing || !modelReady"
      class="absolute size-full flex items-center justify-center bg-black"
    >
      <span class="text-center text-[10vw] text-[#fff]">
        {{ resizing ? $t('pages.main.hints.redrawing') : $t('pages.main.hints.switching') }}
      </span>
    </div>

    <!--
      R44：以前只分「联机没打开」和「对方离线」，于是「没连上服务器 / 正在连 / 连不上」都写成
      「对方离线」——用户会一直等对方，而真实原因可能是自己地址填错或服务器没开。
    -->
    <div
      v-if="stateNote"
      class="absolute inset-x-0 top-0 flex justify-center pt-2"
    >
      <div class="max-w-full break-all rounded-[0.5rem] bg-black/55 px-2 py-1 text-[11px] text-[#fff]">
        {{ $t(stateNote) }}
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

<script setup lang="ts">
import { emit } from '@tauri-apps/api/event'
import { save } from '@tauri-apps/plugin-dialog'
import { useDebounceFn } from '@vueuse/core'
import { Alert, Badge, Button, Flex, Input, InputNumber, message, Modal, Select, Slider, Switch, Tag } from 'antdv-next'
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ExportFormat, HistoryStats } from '@/composables/usePair'

import ProListItem from '@/components/pro-list-item/index.vue'
import ProList from '@/components/pro-list/index.vue'
import {
  ATTACHMENT_MAX_MB,
  pairConnect,
  pairDeleteSecret,
  pairDisconnect,
  pairGetSecretFingerprint,
  pairHasSecret,
  pairHistoryExport,
  pairHistoryStartNewEpoch,
  pairHistoryStats,
  pairSetMaxAttachmentMb,
  pairSetSecret,
} from '@/composables/usePair'
import { chatExportFileName } from '@/composables/usePairChat'
import { playPairMessageSound } from '@/composables/usePairMessageSound'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY } from '@/constants'
import { useModelStore } from '@/stores/model'
import { pairStatusKey, usePairStore } from '@/stores/pair'

const pairStore = usePairStore()
const modelStore = useModelStore()
const { t } = useI18n()
const secretInput = ref('')
const saving = ref(false)
const exportFormat = ref<ExportFormat>('json')
const exporting = ref(false)
const historyStats = ref<HistoryStats>({ epoch: 1, current: 0, total: 0 })

/** §36：保存上限只用来提示进度，达到上限也不会自动删消息 */
const historyLimit = computed(() => {
  const value = Math.round(Number(pairStore.settings.chat.historyMaxMessages))

  return Math.max(1, Number.isFinite(value) ? value : 50_000)
})

const historyPercent = computed(() => {
  return Math.min(100, Math.round(historyStats.value.current / historyLimit.value * 100))
})

const historyWarning = computed(() => {
  if (historyStats.value.current >= historyLimit.value) return 'full'
  if (historyStats.value.current >= historyLimit.value * 0.9) return 'near'

  return ''
})

const exportFormatOptions = computed(() => {
  return (['json', 'txt', 'md'] as const).map(format => ({
    label: t(`pages.preference.pair.options.exportFormat.${format}`),
    value: format,
  }))
})

const exportFilters = computed(() => {
  const name = {
    json: 'JSON',
    txt: 'Text',
    md: 'Markdown',
  }[exportFormat.value]

  return [{ name, extensions: [exportFormat.value] }]
})

const refreshHistoryStatsLater = useDebounceFn(refreshHistoryStats, 2000)

async function refreshHistoryStats() {
  try {
    historyStats.value = await pairHistoryStats()
  } catch (reason) {
    message.error(String(reason))
  }
}

/** 对方发来的消息也要反映到「保存进度」上，聊天时 2 秒刷新一次就够了 */
useTauriListen(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, refreshHistoryStatsLater)

/** §42：附件上限存在偏好里，改完同步给 Rust 侧（发送与接收用的是同一份限制） */
function applyAttachmentLimit(mb: number) {
  pairSetMaxAttachmentMb(mb).catch((reason) => {
    message.error(String(reason))
  })
}

/** 数字框每敲一下都会改值，别每次都 invoke */
const applyAttachmentLimitLater = useDebounceFn(applyAttachmentLimit, 400)

watch(() => pairStore.settings.chat.attachmentMaxMb, (value) => {
  applyAttachmentLimitLater(Number(value))
})

function pickExportPath() {
  return save({
    defaultPath: chatExportFileName(exportFormat.value, Date.now()),
    filters: exportFilters.value,
  })
}

async function handleExportHistory() {
  exporting.value = true

  try {
    const path = await pickExportPath()

    if (!path) return

    const summary = await pairHistoryExport(exportFormat.value, path)

    message.success(t('pages.preference.pair.hints.chatExported', { count: summary.messages }))

    await refreshHistoryStats()
  } catch (reason) {
    message.error(String(reason))
  } finally {
    exporting.value = false
  }
}

/**
 * §36：导出并开始新的记录周期。
 *
 * 先把当前周期导出成文件，导出成功后周期号才 +1；旧记录保留还是删除由用户在
 * 「删除旧记录」开关里决定，任何情况下都不会静默删掉聊天记录。
 */
function handleStartNewEpoch() {
  Modal.confirm({
    title: t('pages.preference.pair.hints.newEpochTitle'),
    content: pairStore.settings.chat.deleteOldOnReset
      ? t('pages.preference.pair.hints.newEpochDeleteOld')
      : t('pages.preference.pair.hints.newEpochKeepOld'),
    okText: t('pages.preference.pair.buttons.exportAndReset'),
    async onOk() {
      try {
        const path = await pickExportPath()

        if (!path) {
          message.info(t('pages.preference.pair.hints.exportCanceled'))

          return
        }

        const summary = await pairHistoryExport(exportFormat.value, path)
        const epoch = await pairHistoryStartNewEpoch(pairStore.settings.chat.deleteOldOnReset)

        message.success(t('pages.preference.pair.hints.newEpochDone', {
          epoch,
          count: summary.messages,
        }))

        await emit(LISTEN_KEY.CHAT_HISTORY_RESET)
        await refreshHistoryStats()
      } catch (reason) {
        message.error(String(reason))
      }
    },
  })
}

// 连接状态由偏好窗口根组件统一订阅（§49 / §50），secret 的「存过没有」只存在于
// 系统凭据库，页面重新挂载时要重新拉一次，否则重启后看不到「已配置」与删除入口
onMounted(async () => {
  // 「存过没有」决定「已配置」标记与删除入口，指纹只用于核对，所以两者分开处理：
  // 指纹读不出来时不要把前者一起吞掉，否则用户会以为自己的 secret 丢了
  await pairHasSecret()
    .then((hasSecret) => {
      pairStore.hasSecret = hasSecret
    })
    .catch((reason) => {
      message.error(String(reason))
    })

  await pairGetSecretFingerprint()
    .then((fingerprint) => {
      pairStore.secretFingerprint = fingerprint ?? ''
    })
    .catch((reason) => {
      message.error(String(reason))
    })

  await refreshHistoryStats()

  // 重启后 Rust 侧回到默认上限，把用户设置补回去
  applyAttachmentLimit(pairStore.settings.chat.attachmentMaxMb)
})

const status = computed(() => {
  const key = pairStatusKey(pairStore.runtime.connection)

  const color = {
    connected: 'success',
    peerOffline: 'warning',
    connecting: 'processing',
    error: 'error',
  }[key] ?? 'default'

  return { key, color } as { key: ReturnType<typeof pairStatusKey>, color: 'success' | 'warning' | 'processing' | 'error' | 'default' }
})

/** P2P 这条腿只做显示（R28）：它掉了不影响聊天、附件、语音，所以这里没有按钮 */
const p2pStatus = computed(() => {
  const key = pairStore.runtime.p2p

  const color = {
    connected: 'success',
    connecting: 'processing',
  }[key] ?? 'default'

  return { key, color } as { key: typeof key, color: 'success' | 'processing' | 'default' }
})

const modelOptions = computed(() => {
  return modelStore.models.map((model) => {
    const current = model.id === modelStore.currentModel?.id

    return {
      label: current ? `${model.mode} ✓` : model.mode,
      value: model.id,
    }
  })
})

const statusText = computed(() => `pages.preference.pair.status.${status.value.key}`)

async function handleSaveSecret() {
  const secret = secretInput.value.trim()

  if (!secret) return

  saving.value = true

  try {
    // Rust 只回显指纹，不回显 secret 本身（R10 / R17）
    pairStore.secretFingerprint = await pairSetSecret(secret)
    pairStore.hasSecret = true
    secretInput.value = ''

    message.success(t('pages.preference.pair.hints.secretSaved'))

    // 换了 secret 之后旧连接上的 token 已经失效：如果本来连着，就用新值重连
    const { connection } = pairStore.runtime
    const wasConnected = ['connecting', 'connected', 'peer-offline', 'reconnecting'].includes(connection)

    if (wasConnected && pairStore.settings.relay.url) {
      pairStore.runtime.lastError = void 0

      await pairDisconnect()
      await pairConnect(pairStore.settings.relay.url)
    }
  } catch (reason) {
    message.error(String(reason))
  } finally {
    saving.value = false
  }
}

async function handleDeleteSecret() {
  try {
    await pairDisconnect()
    await pairDeleteSecret()

    pairStore.hasSecret = false
    pairStore.secretFingerprint = ''
  } catch (reason) {
    message.error(String(reason))
  }
}

async function handleConnect() {
  try {
    pairStore.runtime.lastError = void 0

    await pairConnect(pairStore.settings.relay.url)
  } catch (reason) {
    message.error(String(reason))
  }
}

async function handleDisconnect() {
  try {
    await pairDisconnect()
  } catch (reason) {
    message.error(String(reason))
  }
}

function handlePresenceChange(away: boolean) {
  pairStore.settings.presence = away ? 'away' : 'active'
}

/** §46：偏好页里点一下就能听到提示音，不用等对方发消息（这里的点击也解掉自动播放限制） */
function handlePreviewSound() {
  playPairMessageSound(pairStore.settings.chat.notificationVolume).catch(reason =>
    message.error(String(reason)),
  )
}

/** 声音关掉或音量为 0 时点了也不会有声，直接禁用「试听」 */
const canPreviewSound = computed(
  () =>
    pairStore.settings.chat.notificationSound
    && pairStore.settings.chat.notificationVolume > 0,
)
</script>

<template>
  <ProList :title="$t('pages.preference.pair.labels.connectionSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.enabled')"
      :title="$t('pages.preference.pair.labels.enabled')"
    >
      <Switch v-model:checked="pairStore.settings.enabled" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.relayUrl')"
      :title="$t('pages.preference.pair.labels.relayUrl')"
      vertical
    >
      <Input
        v-model:value="pairStore.settings.relay.url"
        class="w-full"
        :placeholder="$t('pages.preference.pair.placeholders.relayUrl')"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.pairSecret')"
      :title="$t('pages.preference.pair.labels.pairSecret')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="small"
      >
        <Input.Password
          v-model:value="secretInput"
          class="w-60"
          :placeholder="$t('pages.preference.pair.placeholders.pairSecret')"
        />

        <Button
          :disabled="!secretInput.trim()"
          :loading="saving"
          type="primary"
          @click="handleSaveSecret"
        >
          {{ $t('pages.preference.pair.buttons.save') }}
        </Button>

        <Tag
          v-if="pairStore.hasSecret"
          color="success"
        >
          {{ $t('pages.preference.pair.hints.secretConfigured') }}
        </Tag>

        <Button
          v-if="pairStore.hasSecret"
          danger
          type="text"
          @click="handleDeleteSecret"
        >
          {{ $t('pages.preference.pair.buttons.delete') }}
        </Button>
      </Flex>

      <Flex
        v-if="pairStore.secretFingerprint"
        class="mt-2"
        vertical
      >
        <span class="text-3 color-text-tertiary">
          {{ $t('pages.preference.pair.hints.fingerprint') }}
        </span>

        <span class="font-mono">{{ pairStore.secretFingerprint }}</span>
      </Flex>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.autoConnect')"
      :title="$t('pages.preference.pair.labels.autoConnect')"
    >
      <Switch v-model:checked="pairStore.settings.relay.autoConnect" />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.status')">
      <Flex
        align="center"
        gap="small"
      >
        <Badge
          :status="status.color"
          :text="$t(statusText)"
        />

        <Button
          :disabled="!pairStore.settings.enabled"
          size="small"
          @click="handleConnect"
        >
          {{ $t('pages.preference.pair.buttons.connect') }}
        </Button>

        <Button
          :disabled="!pairStore.settings.enabled"
          size="small"
          @click="handleDisconnect"
        >
          {{ $t('pages.preference.pair.buttons.disconnect') }}
        </Button>
      </Flex>
    </ProListItem>

    <ProListItem
      v-if="pairStore.settings.enabled"
      :description="$t('pages.preference.pair.p2p.hint')"
      :title="$t('pages.preference.pair.p2p.label')"
    >
      <Badge
        :status="p2pStatus.color"
        :text="$t(`pages.preference.pair.p2p.${p2pStatus.key}`)"
      />
    </ProListItem>

    <ProListItem
      v-if="pairStore.runtime.lastError"
      :title="$t('pages.preference.pair.labels.lastError')"
      vertical
    >
      <span class="break-all text-3 color-red-5">{{ pairStore.runtime.lastError }}</span>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.remoteCatSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteCat')"
      :title="$t('pages.preference.pair.labels.showRemoteCat')"
    >
      <Switch v-model:checked="pairStore.settings.remoteCat.visible" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteModel')"
      :title="$t('pages.preference.pair.labels.remoteModel')"
    >
      <Select
        v-model:value="pairStore.settings.remoteCat.modelId"
        allow-clear
        class="w-40"
        :options="modelOptions"
        :placeholder="$t('pages.preference.pair.placeholders.remoteModel')"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteStats')"
      :title="$t('pages.preference.pair.labels.remoteStats')"
    >
      <Switch v-model:checked="pairStore.settings.remoteCat.showStats" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.windowSize')"
      :title="$t('pages.preference.pair.labels.windowSize')"
    >
      <Flex align="center">
        <InputNumber
          v-model:value="pairStore.settings.remoteCat.scale"
          class="w-20"
          :max="500"
          :min="10"
        />

        <span class="ml-2">%</span>
      </Flex>
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.opacity')">
      <Slider
        v-model:value="pairStore.settings.remoteCat.opacity"
        class="w-40 m-0!"
        :max="100"
        :min="10"
        :tooltip="{
          formatter(value) {
            return `${value}%`
          },
        }"
      />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.alwaysOnTop')">
      <Switch v-model:checked="pairStore.settings.remoteCat.alwaysOnTop" />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.passThrough')">
      <Switch v-model:checked="pairStore.settings.remoteCat.passThrough" />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.chatSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.chatWindow')"
      :title="$t('pages.preference.pair.labels.showChat')"
    >
      <Switch v-model:checked="pairStore.settings.chat.visible" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.bubbleCount')"
      :title="$t('pages.preference.pair.labels.bubbleCount')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.bubbleCount"
        class="w-20"
        :max="20"
        :min="1"
      />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.chatAlwaysOnTop')">
      <Switch v-model:checked="pairStore.settings.chat.alwaysOnTop" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.chatPassThrough')"
      :title="$t('pages.preference.pair.labels.chatPassThrough')"
    >
      <Switch v-model:checked="pairStore.settings.chat.passThrough" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.notificationSound')"
      :title="$t('pages.preference.pair.labels.notificationSound')"
    >
      <Switch v-model:checked="pairStore.settings.chat.notificationSound" />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.notificationVolume')">
      <Flex
        align="center"
        gap="small"
      >
        <Slider
          v-model:value="pairStore.settings.chat.notificationVolume"
          class="w-40 m-0!"
          :disabled="!pairStore.settings.chat.notificationSound"
          :max="100"
          :min="0"
          :tooltip="{
            formatter(value) {
              return `${value}%`
            },
          }"
        />

        <Button
          :disabled="!canPreviewSound"
          size="small"
          @click="handlePreviewSound"
        >
          {{ $t('pages.preference.pair.buttons.previewSound') }}
        </Button>
      </Flex>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.attachmentLimit')"
      :title="$t('pages.preference.pair.labels.attachmentLimit')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.attachmentMaxMb"
        class="w-24"
        :max="ATTACHMENT_MAX_MB.max"
        :min="ATTACHMENT_MAX_MB.min"
      />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.chatHistory')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.chatHistory')"
      :title="$t('pages.preference.pair.labels.chatHistoryUsage')"
      vertical
    >
      <span>
        {{ $t('pages.preference.pair.labels.chatHistoryProgress', {
          epoch: historyStats.epoch,
          current: historyStats.current,
          limit: historyLimit,
          percent: historyPercent,
        }) }}
      </span>

      <span class="text-3 color-text-tertiary">
        {{ $t('pages.preference.pair.labels.chatHistoryTotal', { total: historyStats.total }) }}
      </span>
    </ProListItem>

    <ProListItem
      v-if="historyWarning"
      vertical
    >
      <Alert
        class="w-full"
        :message="historyWarning === 'full'
          ? $t('pages.preference.pair.hints.historyFull')
          : $t('pages.preference.pair.hints.historyNear')"
        show-icon
        :type="historyWarning === 'full' ? 'error' : 'warning'"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.historyLimit')"
      :title="$t('pages.preference.pair.labels.historyLimit')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.historyMaxMessages"
        class="w-28"
        :max="500000"
        :min="1000"
        :step="1000"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.exportFormat')"
      :title="$t('pages.preference.pair.labels.exportFormat')"
    >
      <Select
        v-model:value="exportFormat"
        class="w-32"
        :options="exportFormatOptions"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.exportChat')"
      :title="$t('pages.preference.pair.labels.exportChat')"
    >
      <Button
        :loading="exporting"
        @click="handleExportHistory"
      >
        {{ $t('pages.preference.pair.buttons.export') }}
      </Button>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.deleteOldOnReset')"
      :title="$t('pages.preference.pair.labels.deleteOldOnReset')"
    >
      <Switch v-model:checked="pairStore.settings.chat.deleteOldOnReset" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.newEpoch')"
      :title="$t('pages.preference.pair.labels.newEpoch')"
    >
      <Button
        danger
        @click="handleStartNewEpoch"
      >
        {{ $t('pages.preference.pair.buttons.newEpoch') }}
      </Button>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.privacySettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.shareTypingActivity')"
      :title="$t('pages.preference.pair.labels.shareTypingActivity')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.shareTypingActivity" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.sharePointer')"
      :title="$t('pages.preference.pair.labels.sharePointer')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.sharePointer" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.shareInputStats')"
      :title="$t('pages.preference.pair.labels.shareInputStats')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.shareInputStats" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.pauseActivitySync')"
      :title="$t('pages.preference.pair.labels.pauseActivitySync')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.pauseActivitySync" />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.statsSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.stats')"
      :title="$t('pages.preference.pair.labels.myInput')"
    >
      <Flex
        align="center"
        class="w-full"
        justify="space-between"
      >
        <span>
          {{ $t('pages.preference.pair.labels.todayInput', {
            keyboard: pairStore.stats.todayKeyboard,
            mouse: pairStore.stats.todayMouse,
          }) }}
        </span>

        <span>
          {{ $t('pages.preference.pair.labels.totalInput', {
            keyboard: pairStore.stats.totalKeyboard,
            mouse: pairStore.stats.totalMouse,
          }) }}
        </span>
      </Flex>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.awaySettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.autoReturn')"
      :title="$t('pages.preference.pair.labels.autoReturn')"
    >
      <Switch v-model:checked="pairStore.settings.away.autoReturn" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.awayMessage')"
      :title="$t('pages.preference.pair.labels.awayMessage')"
      vertical
    >
      <Input
        v-model:value="pairStore.settings.away.message"
        class="w-full"
        :placeholder="$t('pages.preference.pair.placeholders.awayMessage')"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.sendSystemNotice')"
      :title="$t('pages.preference.pair.labels.sendSystemNotice')"
    >
      <Switch v-model:checked="pairStore.settings.away.sendSystemNotice" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.presence')"
      :title="$t('pages.preference.pair.labels.presence')"
    >
      <Switch
        :checked="pairStore.settings.presence === 'away'"
        @update:checked="handlePresenceChange"
      />
    </ProListItem>
  </ProList>
</template>

<script setup lang="ts">
import { Badge, Button, Flex, Input, InputNumber, message, Select, Slider, Switch, Tag } from 'antdv-next'
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import ProListItem from '@/components/pro-list-item/index.vue'
import ProList from '@/components/pro-list/index.vue'
import {
  pairConnect,
  pairDeleteSecret,
  pairDisconnect,
  pairGetSecretFingerprint,
  pairHasSecret,
  pairSetSecret,
} from '@/composables/usePair'
import { useModelStore } from '@/stores/model'
import { pairStatusKey, usePairStore } from '@/stores/pair'

const pairStore = usePairStore()
const modelStore = useModelStore()
const { t } = useI18n()
const secretInput = ref('')
const saving = ref(false)

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

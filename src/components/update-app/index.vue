<script setup lang="ts">
import { openUrl } from '@tauri-apps/plugin-opener'
import { useIntervalFn } from '@vueuse/core'
import { message, Modal } from 'antdv-next'
import { ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import { useTauriListen } from '@/composables/useTauriListen'
import { GITHUB_LINK, LISTEN_KEY } from '@/constants'
import { showWindow } from '@/plugins/window'
import { useAppStore } from '@/stores/app'
import { useGeneralStore } from '@/stores/general'
import { latestClientTag } from '@/utils/releaseVersion'

const general = useGeneralStore()
const app = useAppStore()
const { t } = useI18n()
const open = ref(false)
const nextTag = ref('')
let checking = false

// 自用包未签名：只检查本 fork 的客户端版本，下载由用户在 Releases 中选择。
async function checkUpdate(manual = false) {
  if (checking) return
  checking = true
  if (manual) message.loading({ key: 'update', duration: 0, content: t('components.updateApp.hints.checkingUpdates') })

  try {
    const response = await fetch('https://api.github.com/repos/CATMIAOZHI/BongoCat/releases?per_page=100', {
      signal: AbortSignal.timeout(10000),
      headers: { Accept: 'application/vnd.github+json' },
    })
    if (!response.ok) throw new Error(`GitHub HTTP ${response.status}`)
    const tag = latestClientTag(await response.json(), app.version)
    if (tag) {
      nextTag.value = tag
      open.value = true
      await showWindow()
      message.destroy('update')
    } else if (manual) {
      message.success({ key: 'update', content: t('components.updateApp.hints.alreadyLatest') })
    }
  } catch {
    if (manual) {
      message.warning({ key: 'update', content: t('components.updateApp.hints.openReleases') })
      await openUrl(`${GITHUB_LINK}/releases`).catch(() => void 0)
    }
  } finally {
    checking = false
  }
}

const { pause, resume } = useIntervalFn(() => checkUpdate(), 86400000, { immediate: false })
watch(() => general.update.autoCheck, (enabled) => {
  pause()
  if (enabled) {
    void checkUpdate()
    resume()
  }
}, { immediate: true })

useTauriListen(LISTEN_KEY.UPDATE_APP, () => void checkUpdate(true))

async function download() {
  try {
    await openUrl(`${GITHUB_LINK}/releases/tag/${encodeURIComponent(nextTag.value)}`)
    open.value = false
  } catch (reason) {
    message.error(String(reason))
  }
}
</script>

<template>
  <Modal
    v-model:open="open"
    :cancel-text="$t('components.updateApp.buttons.updateLater')"
    :ok-text="$t('components.updateApp.buttons.openDownload')"
    :title="$t('components.updateApp.title')"
    @ok="download"
  >
    <p>{{ app.version }} → {{ nextTag }}</p>
    <p>{{ $t('components.updateApp.hints.manualInstall') }}</p>
  </Modal>
</template>

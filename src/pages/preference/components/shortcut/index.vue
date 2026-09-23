<script setup lang="ts">
import { emit } from '@tauri-apps/api/event'
import { error } from '@tauri-apps/plugin-log'
import { storeToRefs } from 'pinia'

import ProListItem from '@/components/pro-list-item/index.vue'
import ProList from '@/components/pro-list/index.vue'
import Shortcut from '@/components/shortcut/index.vue'
import { useKeyPress } from '@/composables/useKeyPress'
import { LISTEN_KEY, WINDOW_LABEL } from '@/constants'
import { showWindowByLabel, toggleWindowVisible } from '@/plugins/window'
import { useCatStore } from '@/stores/cat'
import { usePairStore } from '@/stores/pair'
import { useShortcutStore } from '@/stores/shortcut.ts'

const shortcutStore = useShortcutStore()
const {
  visibleCat,
  visiblePreference,
  visibleRemoteCat,
  visibleChat,
  toggleChatInput,
  mirrorMode,
  penetrable,
  alwaysOnTop,
  toggleAway,
} = storeToRefs(shortcutStore)
const catStore = useCatStore()
const pairStore = usePairStore()

useKeyPress(visibleCat, () => {
  catStore.window.visible = !catStore.window.visible
})

useKeyPress(visiblePreference, () => {
  toggleWindowVisible(WINDOW_LABEL.PREFERENCE)
})

useKeyPress(mirrorMode, () => {
  catStore.model.mirror = !catStore.model.mirror
})

useKeyPress(penetrable, () => {
  catStore.window.passThrough = !catStore.window.passThrough
})

useKeyPress(alwaysOnTop, () => {
  catStore.window.alwaysOnTop = !catStore.window.alwaysOnTop
})

useKeyPress(visibleRemoteCat, () => {
  pairStore.settings.remoteCat.visible = !pairStore.settings.remoteCat.visible
})

useKeyPress(visibleChat, () => {
  pairStore.settings.chat.visible = !pairStore.settings.chat.visible
})

/**
 * §30 的输入模式：先把聊天窗口叫出来并聚焦（不然打不了字），再让聊天窗口自己切换
 * 输入状态——输入模式是聊天窗口的局部状态，跨窗口只能靠应用级事件通知它。
 */
useKeyPress(toggleChatInput, async () => {
  try {
    pairStore.settings.chat.visible = true

    await showWindowByLabel(WINDOW_LABEL.CHAT, true)
    await emit(LISTEN_KEY.CHAT_INPUT_TOGGLE)
  } catch (reason) {
    error(String(reason))
  }
})

// 暂离的发送由猫咪窗口统一负责（它一直活着，也负责自动回来）
useKeyPress(toggleAway, () => {
  pairStore.settings.presence = pairStore.settings.presence === 'away' ? 'active' : 'away'
})
</script>

<template>
  <ProList :title="$t('pages.preference.shortcut.title')">
    <ProListItem
      :description="$t('pages.preference.shortcut.hints.toggleCat')"
      :title="$t('pages.preference.shortcut.labels.toggleCat')"
    >
      <Shortcut v-model="shortcutStore.visibleCat" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.togglePreferences')"
      :title="$t('pages.preference.shortcut.labels.togglePreferences')"
    >
      <Shortcut v-model="shortcutStore.visiblePreference" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.mirrorMode')"
      :title="$t('pages.preference.shortcut.labels.mirrorMode')"
    >
      <Shortcut v-model="shortcutStore.mirrorMode" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.passThrough')"
      :title="$t('pages.preference.shortcut.labels.passThrough')"
    >
      <Shortcut v-model="shortcutStore.penetrable" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.alwaysOnTop')"
      :title="$t('pages.preference.shortcut.labels.alwaysOnTop')"
    >
      <Shortcut v-model="shortcutStore.alwaysOnTop" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.visibleRemoteCat')"
      :title="$t('pages.preference.shortcut.labels.visibleRemoteCat')"
    >
      <Shortcut v-model="shortcutStore.visibleRemoteCat" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.visibleChat')"
      :title="$t('pages.preference.shortcut.labels.visibleChat')"
    >
      <Shortcut v-model="shortcutStore.visibleChat" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.toggleChatInput')"
      :title="$t('pages.preference.shortcut.labels.toggleChatInput')"
    >
      <Shortcut v-model="shortcutStore.toggleChatInput" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.pushToTalk')"
      :title="$t('pages.preference.shortcut.labels.pushToTalk')"
    >
      <Shortcut v-model="shortcutStore.pushToTalk" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.shortcut.hints.toggleAway')"
      :title="$t('pages.preference.shortcut.labels.toggleAway')"
    >
      <Shortcut v-model="shortcutStore.toggleAway" />
    </ProListItem>
  </ProList>
</template>

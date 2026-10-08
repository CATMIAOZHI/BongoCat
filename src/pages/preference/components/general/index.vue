<script setup lang="ts">
import { Alert, Switch } from 'antdv-next'

import ProListItem from '@/components/pro-list-item/index.vue'
import ProList from '@/components/pro-list/index.vue'
import { useAutostart } from '@/composables/useAutostart'
import { useGeneralStore } from '@/stores/general'
import { isMac, isWindows } from '@/utils/platform'

import Language from './components/language/index.vue'
import MacosPermissions from './components/macos-permissions/index.vue'
import ThemeMode from './components/theme-mode/index.vue'
import WindowsPermissions from './components/windows-permissions/index.vue'

const generalStore = useGeneralStore()
const { busy, failure, apply } = useAutostart()
</script>

<template>
  <MacosPermissions v-if="isMac" />

  <WindowsPermissions v-if="isWindows" />

  <ProList :title="$t('pages.preference.general.labels.appSettings')">
    <ProListItem :title="$t('pages.preference.general.labels.launchOnStartup')">
      <Switch
        :checked="generalStore.app.autostart"
        :disabled="busy"
        :loading="busy"
        @change="value => apply(Boolean(value))"
      />
    </ProListItem>
    <Alert
      v-if="failure"
      :message="failure"
      show-icon
      type="error"
    />

    <ProListItem
      :description="$t('pages.preference.general.hints.showTaskbarIcon')"
      :title="$t('pages.preference.general.labels.showTaskbarIcon')"
    >
      <Switch v-model:checked="generalStore.app.taskbarVisible" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.general.hints.showTrayIcon')"
      :title="$t('pages.preference.general.labels.showTrayIcon')"
    >
      <Switch v-model:checked="generalStore.app.trayVisible" />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.general.labels.appearanceSettings')">
    <ThemeMode />

    <Language />
  </ProList>

  <ProList :title="$t('pages.preference.general.labels.updateSettings')">
    <ProListItem :title="$t('pages.preference.general.labels.autoCheckUpdate')">
      <Switch v-model:checked="generalStore.update.autoCheck" />
    </ProListItem>
  </ProList>
</template>

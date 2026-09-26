<script setup lang="ts">
import { QuestionOutlined } from '@antdv-next/icons'
import { openUrl } from '@tauri-apps/plugin-opener'
import { FloatButton } from 'antdv-next'

import { GITHUB_LINK } from '@/constants'
</script>

<template>
  <FloatButton
    class="bottom-4 right-4"
    :tooltip="$t('pages.preference.model.tooltips.modelGuide')"
    @click="openUrl(`${GITHUB_LINK}#模型`)"
  >
    <template #icon>
      <QuestionOutlined />
    </template>
  </FloatButton>
</template>

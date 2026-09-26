<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { remove } from '@tauri-apps/plugin-fs'
import { revealItemInDir } from '@tauri-apps/plugin-opener'
import { useElementSize } from '@vueuse/core'
import { Card, Input, Masonry, message, Modal, Popconfirm } from 'antdv-next'
import { computed, ref, useTemplateRef } from 'vue'
import { useI18n } from 'vue-i18n'

import type { Model } from '@/stores/model'

import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { join } from '@/utils/path'

import BehaviorModal from './components/behavior-modal/index.vue'
import FloatMenu from './components/float-menu/index.vue'
import Upload from './components/upload/index.vue'

const catStore = useCatStore()
const modelStore = useModelStore()
const firstCardRef = useTemplateRef('firstCard')
const { height } = useElementSize(firstCardRef)
const { t } = useI18n()
const openBehaviorModal = ref(false)
const namingModel = ref<string>()
const nameDraft = ref('')

function editName(model: Model) {
  namingModel.value = model.id
  nameDraft.value = model.name ?? ''
}

function saveName() {
  const name = nameDraft.value.trim()
  if (!name || /[/\\]/.test(name) || Array.from(name).length > 255) {
    message.warning(t('pages.preference.model.hints.modelNameRequired'))
    return
  }
  const model = modelStore.models.find(item => item.id === namingModel.value)
  if (model) {
    model.name = name
    if (modelStore.currentModel?.id === model.id) modelStore.currentModel = { ...model }
  }
  namingModel.value = void 0
}

const masonryItems = computed(() => {
  const items = modelStore.models.map((item) => {
    return {
      key: item.id,
      data: item,
    }
  })

  return [{ key: 'upload', data: null }, ...items]
})

function handleToggle(nextModel: Model) {
  if (modelStore.currentModel?.id === nextModel.id) return

  modelStore.modelReady = false

  modelStore.currentModel = nextModel
}

async function handleDelete(item: Model) {
  const { id, path } = item

  try {
    await remove(path, { recursive: true })

    message.success(t('pages.preference.model.hints.deleteSuccess'))
  } catch (error) {
    message.error(String(error))
  } finally {
    modelStore.models = modelStore.models.filter(item => item.id !== id)

    if (id === modelStore.currentModel?.id) {
      modelStore.currentModel = modelStore.models[0]
    }
  }
}
</script>

<template>
  <Masonry
    :columns="{ xs: 3, lg: 4, xxl: 6 }"
    :gutter="16"
    :items="masonryItems"
  >
    <template #itemRender="{ data, index }">
      <template v-if="!data">
        <Upload :style="{ height: `${height}px` }" />
      </template>

      <Card
        v-else
        :ref="index === 1 ? 'firstCard' : void 0"
        :classes="{
          actions: `[&>li]:(flex justify-center) [&>li>span]:(inline-flex! justify-center text-4!)`,
        }"
        hoverable
        size="small"
        @click="handleToggle(data)"
      >
        <template #title>
          <span
            class="block truncate"
            :title="data.name || data.mode"
          >
            {{ data.isPreset ? data.mode : data.name || $t('pages.preference.model.labels.unnamedModel') }}
          </span>
        </template>
        <template #cover>
          <img
            alt="example"
            :src="convertFileSrc(join(data.path, 'resources', 'cover.png'))"
          >
        </template>

        <template #actions>
          <i
            class="i-lucide:circle-check"
            :class="{ 'text-success': data.id === modelStore.currentModel?.id }"
          />

          <i
            v-if="catStore.model.behavior && modelStore.currentModel?.id === data.id"
            class="i-lucide:smile"
            @click.stop="openBehaviorModal = true"
          />

          <i
            class="i-lucide:folder-open"
            @click.stop="revealItemInDir(data.path)"
          />

          <template v-if="!data.isPreset">
            <button
              :aria-label="$t('pages.preference.model.labels.modelName')"
              :title="$t('pages.preference.model.labels.modelName')"
              type="button"
              @click.stop="editName(data)"
            >
              <i class="i-lucide:pencil" />
            </button>
            <Popconfirm
              :description="$t('pages.preference.model.hints.deleteModel')"
              placement="topRight"
              :title="$t('pages.preference.model.labels.deleteModel')"
              @confirm="handleDelete(data)"
            >
              <i
                class="i-lucide:trash-2"
                @click.stop
              />
            </Popconfirm>
          </template>
        </template>
      </Card>
    </template>
  </Masonry>

  <FloatMenu />

  <Modal
    :open="!!namingModel"
    :title="$t('pages.preference.model.labels.modelName')"
    @cancel="namingModel = undefined"
    @ok="saveName"
  >
    <p class="mb-3">
      {{ $t('pages.preference.model.hints.modelName') }}
    </p>
    <Input
      v-model:value="nameDraft"
      :maxlength="255"
      @press-enter="saveName"
    />
  </Modal>

  <BehaviorModal
    v-if="catStore.model.behavior"
    v-model="openBehaviorModal"
  />
</template>

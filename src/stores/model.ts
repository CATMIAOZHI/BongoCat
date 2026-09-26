import type { ExpressionInfo, MotionInfo } from 'easy-live2d'

import { resolveResource } from '@tauri-apps/api/path'
import { filter, find } from 'es-toolkit/compat'
import { nanoid } from 'nanoid'
import { defineStore } from 'pinia'
import { reactive, ref } from 'vue'

import { join } from '@/utils/path'
import { createBackendSyncGuard } from '@/utils/tauriStoreSync'

export type ModelMode = 'standard' | 'keyboard' | 'gamepad'

/**
 * model store 里「窗口本地」的键：贴图索引与当前按着的键。
 *
 * 这几个键每个窗口各有一份（对方猫那个窗口还会按对方的活动改它们，最高 60Hz），
 * 既不发给后端也不落盘。
 */
const MODEL_LOCAL_STATE_KEYS = ['supportKeys', 'pressedKeys', 'heldKeys']

/** 发之前先比一下：只有模型/快捷键这些真的变了才发（见 `utils/tauriStoreSync.ts`） */
const modelSync = createBackendSyncGuard(MODEL_LOCAL_STATE_KEYS)

export interface Model {
  id: string
  path: string
  mode: ModelMode
  isPreset: boolean
}

export const useModelStore = defineStore('model', () => {
  const modelReady = ref(true)
  const models = ref<Model[]>([])
  const currentModel = ref<Model>()
  const supportKeys = reactive<Record<string, string>>({})
  const pressedKeys = reactive<Record<string, string>>({})
  /**
   * 当前**真的按着**的键（原始键名 → 贴图路径），用来在松开时回退显示。
   *
   * 模型同一时刻只能显示一张键盘贴图，所以 `pressedKeys` 每个贴图目录只留一个键；
   * 但同一个目录里可能同时按着好几个键（先按住 w 再按 e），那份事实记在这里，
   * 否则松开 e 之后 w 就回不来了。
   */
  const heldKeys = reactive<Record<string, string>>({})
  const currentMotions = ref<Array<[string, MotionInfo[]]>>([])
  const currentExpressions = ref<ExpressionInfo[]>([])
  const shortcuts = reactive<Record<string, string>>({})

  const init = async () => {
    const modelsPath = await resolveResource('assets/models')

    const nextModels = filter(models.value, { isPreset: false })
    const presetModels = filter(models.value, { isPreset: true })

    const modes: ModelMode[] = ['gamepad', 'keyboard', 'standard']

    for (const mode of modes) {
      const matched = find(presetModels, { mode })

      nextModels.unshift({
        id: matched?.id ?? nanoid(),
        mode,
        isPreset: true,
        path: join(modelsPath, mode),
      })
    }

    const matched = find(nextModels, { id: currentModel.value?.id })

    currentModel.value = matched ?? nextModels[0]

    models.value = nextModels
  }

  return {
    modelReady,
    models,
    currentModel,
    supportKeys,
    pressedKeys,
    heldKeys,
    currentMotions,
    currentExpressions,
    shortcuts,
    init,
  }
}, {
  tauri: {
    filterKeys: MODEL_LOCAL_STATE_KEYS,
    hooks: {
      /** 载入或别的窗口发来时记下「后端现在长这样」，给下面那个判据当基准 */
      beforeFrontendSync: (state) => {
        modelSync.remember(state)

        return state
      },
      /**
       * 只有窗口本地的按键高亮变了就什么都不发。
       *
       * 不做这一步的话，每敲一次键（对方猫那个窗口是对方的每一次按键）都会把**整份状态**
       * 发给后端：载荷里带着 `shortcuts` / `models` / `currentModel`，而偏好页正好在编辑
       * 这几项——后到的那一帧就会把用户刚改的模型、快捷键顶回去；顺带每次按键都重写一遍
       * `model.json`。
       */
      beforeBackendSync: state => modelSync.sync(state),
    },
  },
})

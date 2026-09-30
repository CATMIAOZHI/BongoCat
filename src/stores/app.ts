import { getName, getVersion } from '@tauri-apps/api/app'
import { defineStore } from 'pinia'
import { reactive, ref } from 'vue'

import type { WindowState } from '@/composables/useWindowState'

export const useAppStore = defineStore('app', () => {
  const name = ref('')
  const version = ref('')
  const windowState = reactive<WindowState>({})

  const init = async () => {
    name.value = await getName()
    version.value = await getVersion()
  }

  return {
    name,
    version,
    windowState,
    init,
  }
}, {
  tauri: {
    /**
     * 落盘改成尾随防抖 1 秒。
     *
     * `windowState` 每次窗口移动 / 缩放都会变（拖动就是每秒几十次），默认策略是「一变就写
     * 一次 JSON」。位置这种数据晚一秒落盘没有代价，而拖动时那几十次写盘是白花的。
     *
     * 注意它只管**落盘**：跨窗口的状态广播（`emit_state_change` → 别的窗口 `$patch`）仍按每个
     * 事件照发，这一次改动没有省掉 IPC。真要把广播也省掉得在这个 store 上另配 `syncStrategy`。
     */
    saveStrategy: 'debounce',
    saveInterval: 1000,
  },
})

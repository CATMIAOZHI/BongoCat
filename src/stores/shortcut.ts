import { defineStore } from 'pinia'
import { ref } from 'vue'

export type HotKey = 'visibleCat' | 'mirrorMode' | 'penetrable' | 'alwaysOnTop'

export const useShortcutStore = defineStore('shortcut', () => {
  const visibleCat = ref('')
  const visiblePreference = ref('')
  const visibleRemoteCat = ref('')
  const mirrorMode = ref('')
  const penetrable = ref('')
  const alwaysOnTop = ref('')
  const toggleAway = ref('')

  return {
    visibleCat,
    visiblePreference,
    visibleRemoteCat,
    mirrorMode,
    penetrable,
    alwaysOnTop,
    toggleAway,
  }
})

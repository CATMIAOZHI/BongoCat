import { defineStore } from 'pinia'
import { ref } from 'vue'

export type HotKey
  = | 'visibleCat'
    | 'visiblePreference'
    | 'visibleRemoteCat'
    | 'visibleChat'
    | 'toggleChatInput'
    | 'mirrorMode'
    | 'penetrable'
    | 'alwaysOnTop'
    | 'toggleAway'

export const useShortcutStore = defineStore('shortcut', () => {
  const visibleCat = ref('')
  const visiblePreference = ref('')
  const visibleRemoteCat = ref('')
  const visibleChat = ref('')
  const toggleChatInput = ref('')
  const mirrorMode = ref('')
  const penetrable = ref('')
  const alwaysOnTop = ref('')
  const toggleAway = ref('')

  return {
    visibleCat,
    visiblePreference,
    visibleRemoteCat,
    visibleChat,
    toggleChatInput,
    mirrorMode,
    penetrable,
    alwaysOnTop,
    toggleAway,
  }
})

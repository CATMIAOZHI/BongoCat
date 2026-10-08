import { invoke } from '@tauri-apps/api/core'
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart'
import { ref } from 'vue'

import { useGeneralStore } from '@/stores/general'
import { isWindows } from '@/utils/platform'

const busy = ref(false)
const failure = ref('')

export function useAutostart() {
  const store = useGeneralStore()

  async function apply(enabled: boolean) {
    if (busy.value) return
    busy.value = true
    failure.value = ''
    try {
      if (isWindows) {
        await invoke('configure_autostart', { enabled })
      } else if (await isEnabled() !== enabled) {
        await (enabled ? enable() : disable())
      }
      store.app.autostart = enabled
    } catch (error) {
      failure.value = String(error)
    } finally {
      busy.value = false
    }
  }

  return { busy, failure, apply }
}

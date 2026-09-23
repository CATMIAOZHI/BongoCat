import type { ShortcutHandler } from '@tauri-apps/plugin-global-shortcut'
import type { Ref } from 'vue'

import {
  isRegistered,
  register,
  unregister,
} from '@tauri-apps/plugin-global-shortcut'
import { error } from '@tauri-apps/plugin-log'
import { onUnmounted, ref, watch } from 'vue'

/**
 * 需要区分「按下」与「松开」的快捷键（§48 的 Push-To-Talk）。
 *
 * `useKeyPress` 会把 Released 过滤掉，按住说话必须两个状态都收到，所以单独一个
 * composable：回调拿到 `true` 表示按下、`false` 表示松开。
 */
export function useKeyStateShortcut(
  shortcut: Ref<string | undefined, string>,
  callback: (pressed: boolean) => void,
) {
  const oldShortcut = ref(shortcut.value)

  async function unbind() {
    if (!oldShortcut.value) return

    const registered = await isRegistered(oldShortcut.value)

    if (!registered) return

    return unregister(oldShortcut.value)
  }

  watch(shortcut, async (value) => {
    await unbind()

    if (!value) return

    const handler: ShortcutHandler = (event) => {
      callback(event.state === 'Pressed')
    }

    // 注册失败只记日志：那通常是「这个键被别的程序占了」，抛出去会变成
    // 一个没人处理的 Promise，用户什么提示也看不到
    try {
      await register(value, handler)
    } catch (reason) {
      error(`注册按住说话快捷键失败: ${String(reason)}`)

      return
    }

    oldShortcut.value = value
  }, { immediate: true })

  onUnmounted(unbind)
}

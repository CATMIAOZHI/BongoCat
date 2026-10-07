import { Menu, PredefinedMenuItem } from '@tauri-apps/api/menu'
import { error } from '@tauri-apps/plugin-log'

import { setAlwaysOnTop } from '@/plugins/window'
import { useCatStore } from '@/stores/cat'
import { usePairStore } from '@/stores/pair'
import { isWindows } from '@/utils/platform'

import type { MenuResources } from './useAppMenu'

import { closeMenuResources, trackMenuResource, useAppMenu } from './useAppMenu'

export function useCatContextMenu(target: 'main' | 'remote') {
  const cat = useCatStore()
  const pair = usePairStore()
  const resources: MenuResources = []
  const { getBaseMenu, getExitMenu } = useAppMenu(resources)
  const pinned = () => target === 'main' ? cat.window.alwaysOnTop : pair.settings.remoteCat.alwaysOnTop
  let opened = false

  return async () => {
    if (opened) return
    opened = true
    try {
      const menu = await trackMenuResource(Menu.new({
        items: [
          ...await getBaseMenu(target),
          await trackMenuResource(PredefinedMenuItem.new({ item: 'Separator' }), resources),
          ...await getExitMenu(),
        ],
      }), resources)
      try {
        if (isWindows && pinned()) await setAlwaysOnTop(false)
        await menu.popup()
      } finally {
        // A menu action may have changed the preference while the popup was open.
        if (isWindows) await setAlwaysOnTop(pinned())
      }
    } catch (reason) {
      error(`猫咪右键菜单打开失败: ${String(reason)}`)
    } finally {
      await closeMenuResources(resources)
      opened = false
    }
  }
}

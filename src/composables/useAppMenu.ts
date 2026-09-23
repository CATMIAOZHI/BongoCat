import { CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu } from '@tauri-apps/api/menu'
import { error } from '@tauri-apps/plugin-log'
import { exit, relaunch } from '@tauri-apps/plugin-process'
import { range } from 'es-toolkit'
import { useI18n } from 'vue-i18n'

import { WINDOW_LABEL } from '@/constants'
import { isWindowVisible, showWindow, toggleWindowVisibleByLabel } from '@/plugins/window'
import { useCatStore } from '@/stores/cat'
import { pairStatusKey, usePairStore } from '@/stores/pair'
import { isMac } from '@/utils/platform'

export function useAppMenu() {
  const catStore = useCatStore()
  const pairStore = usePairStore()
  const { t } = useI18n()

  const getScaleMenuItems = async () => {
    const options = range(50, 151, 25)

    const items = options.map((item) => {
      return CheckMenuItem.new({
        text: `${item}%`,
        checked: catStore.window.scale === item,
        action: () => {
          catStore.window.scale = item
        },
      })
    })

    if (!options.includes(catStore.window.scale)) {
      items.unshift(CheckMenuItem.new({
        text: `${catStore.window.scale}%`,
        checked: true,
        enabled: false,
      }))
    }

    return Promise.all(items)
  }

  const getOpacityMenuItems = async () => {
    const options = range(25, 101, 25)

    const items = options.map((item) => {
      return CheckMenuItem.new({
        text: `${item}%`,
        checked: catStore.window.opacity === item,
        action: () => {
          catStore.window.opacity = item
        },
      })
    })

    if (!options.includes(catStore.window.opacity)) {
      items.unshift(CheckMenuItem.new({
        text: `${catStore.window.opacity}%`,
        checked: true,
        enabled: false,
      }))
    }

    return Promise.all(items)
  }

  const getBaseMenu = async () => {
    const chatVisible = await isWindowVisible(WINDOW_LABEL.CHAT).catch(() => false)

    return await Promise.all([
      MenuItem.new({
        text: t('composables.useAppMenu.labels.preference'),
        accelerator: isMac ? 'Cmd+,' : '',
        action: () => showWindow(WINDOW_LABEL.PREFERENCE),
      }),
      MenuItem.new({
        text: catStore.window.visible ? t('composables.useAppMenu.labels.hideCat') : t('composables.useAppMenu.labels.showCat'),
        action: () => {
          catStore.window.visible = !catStore.window.visible
        },
      }),
      MenuItem.new({
        text: pairStore.settings.remoteCat.visible ? t('composables.useAppMenu.labels.hideRemoteCat') : t('composables.useAppMenu.labels.showRemoteCat'),
        action: () => {
          pairStore.settings.remoteCat.visible = !pairStore.settings.remoteCat.visible
        },
      }),
      // §53：右键菜单里给出暂离入口与当前连接状态，完整配置仍然只在偏好页
      MenuItem.new({
        text: pairStore.settings.presence === 'away' ? t('composables.useAppMenu.labels.backToActive') : t('composables.useAppMenu.labels.stepAway'),
        action: () => {
          pairStore.settings.presence = pairStore.settings.presence === 'away' ? 'active' : 'away'
        },
      }),
      ...(pairStore.settings.enabled
        ? [
            MenuItem.new({
              text: t(`pages.preference.pair.status.${pairStatusKey(pairStore.runtime.connection)}`),
              enabled: false,
            }),
          ]
        : []),
      MenuItem.new({
        text: chatVisible ? t('composables.useAppMenu.labels.hideChat') : t('composables.useAppMenu.labels.showChat'),
        action: () => {
          toggleWindowVisibleByLabel(WINDOW_LABEL.CHAT, true).catch(reason => error(String(reason)))
        },
      }),
      PredefinedMenuItem.new({ item: 'Separator' }),
      CheckMenuItem.new({
        text: t('composables.useAppMenu.labels.passThrough'),
        checked: catStore.window.passThrough,
        action: () => {
          catStore.window.passThrough = !catStore.window.passThrough
        },
      }),
      Submenu.new({
        text: t('composables.useAppMenu.labels.windowSize'),
        items: await getScaleMenuItems(),
      }),
      Submenu.new({
        text: t('composables.useAppMenu.labels.opacity'),
        items: await getOpacityMenuItems(),
      }),
    ])
  }

  const getExitMenu = async () => {
    return await Promise.all([
      MenuItem.new({
        text: t('composables.useAppMenu.labels.restartApp'),
        action: relaunch,
      }),
      MenuItem.new({
        text: t('composables.useAppMenu.labels.quitApp'),
        accelerator: isMac ? 'Cmd+Q' : '',
        action: () => exit(0),
      }),
    ])
  }

  return {
    getBaseMenu,
    getExitMenu,
  }
}

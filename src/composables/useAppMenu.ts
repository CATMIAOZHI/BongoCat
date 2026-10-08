import { CheckMenuItem as NativeCheckMenuItem, MenuItem as NativeMenuItem, PredefinedMenuItem as NativePredefinedMenuItem, Submenu as NativeSubmenu } from '@tauri-apps/api/menu'
import { range } from 'es-toolkit'
import { useI18n } from 'vue-i18n'

import { setChatVisible, setRemoteCatVisible } from '@/composables/usePairOverlay'
import { WINDOW_LABEL } from '@/constants'
import { showWindow } from '@/plugins/window'
import { useCatStore } from '@/stores/cat'
import { pairStatusKey, usePairStore } from '@/stores/pair'
import { isMac } from '@/utils/platform'

export type MenuResources = Promise<{ close: () => Promise<void> }>[]

/** Record pending creations too, so a partial construction failure is safe to clean up. */
export function trackMenuResource<T extends { close: () => Promise<void> }>(resource: Promise<T>, resources?: MenuResources): Promise<T> {
  resources?.push(resource)
  return resource
}

function trackedFactory<O, T extends { close: () => Promise<void> }>(factory: { new: (options: O) => Promise<T> }, resources?: MenuResources) {
  return { new: (options: O) => trackMenuResource(factory.new(options), resources) }
}

export async function closeMenuResources(resources: MenuResources) {
  const results = await Promise.allSettled(resources.splice(0))
  await Promise.allSettled(results.reverse().map(result => result.status === 'fulfilled' ? result.value.close() : Promise.resolve()))
}

export function useAppMenu(resources?: MenuResources) {
  const MenuItem = trackedFactory(NativeMenuItem, resources)
  const CheckMenuItem = trackedFactory(NativeCheckMenuItem, resources)
  const PredefinedMenuItem = trackedFactory(NativePredefinedMenuItem, resources)
  const Submenu = trackedFactory(NativeSubmenu, resources)
  const catStore = useCatStore()
  const pairStore = usePairStore()
  const { t } = useI18n()

  const windowSettings = (target: 'main' | 'remote') => target === 'remote'
    ? pairStore.settings.remoteCat
    : catStore.window

  const getScaleMenuItems = async (target: 'main' | 'remote') => {
    const options = range(50, 151, 25)

    const items = options.map((item) => {
      return CheckMenuItem.new({
        text: `${item}%`,
        checked: windowSettings(target).scale === item,
        action: () => {
          windowSettings(target).scale = item
        },
      })
    })

    if (!options.includes(windowSettings(target).scale)) {
      items.unshift(CheckMenuItem.new({
        text: `${windowSettings(target).scale}%`,
        checked: true,
        enabled: false,
      }))
    }

    return Promise.all(items)
  }

  const getOpacityMenuItems = async (target: 'main' | 'remote') => {
    const options = range(25, 101, 25)

    const items = options.map((item) => {
      return CheckMenuItem.new({
        text: `${item}%`,
        checked: windowSettings(target).opacity === item,
        action: () => {
          windowSettings(target).opacity = item
        },
      })
    })

    if (!options.includes(windowSettings(target).opacity)) {
      items.unshift(CheckMenuItem.new({
        text: `${windowSettings(target).opacity}%`,
        checked: true,
        enabled: false,
      }))
    }

    return Promise.all(items)
  }

  const getBaseMenu = async (target: 'main' | 'remote' = 'main') => {
    return await Promise.all([
      MenuItem.new({
        text: t('composables.useAppMenu.labels.preference'),
        accelerator: isMac ? 'Cmd+,' : '',
        action: () => showWindow(WINDOW_LABEL.PREFERENCE),
      }),
      ...(target === 'main'
        ? [MenuItem.new({
            text: catStore.window.visible ? t('composables.useAppMenu.labels.hideCat') : t('composables.useAppMenu.labels.showCat'),
            action: () => {
              catStore.window.visible = !catStore.window.visible
            },
          })]
        : []),
      MenuItem.new({
        text: pairStore.settings.remoteCat.visible ? t('composables.useAppMenu.labels.hideRemoteCat') : t('composables.useAppMenu.labels.showRemoteCat'),
        action: () => {
          setRemoteCatVisible(!pairStore.settings.remoteCat.visible)
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
        // 和对方猫一样以 store 里的开关为准，窗口由改开关的这个窗口直接切
        text: pairStore.settings.chat.visible ? t('composables.useAppMenu.labels.hideChat') : t('composables.useAppMenu.labels.showChat'),
        action: () => {
          setChatVisible(!pairStore.settings.chat.visible)
        },
      }),
      ...(target === 'main'
        ? [CheckMenuItem.new({
            text: t('pages.preference.pair.labels.chatOverlay'),
            checked: pairStore.settings.chat.overlayVisible !== false,
            action: () => {
              pairStore.settings.chat.overlayVisible = pairStore.settings.chat.overlayVisible === false
            },
          })]
        : [
            CheckMenuItem.new({
              text: t('pages.preference.pair.labels.remoteStats'),
              checked: pairStore.settings.remoteCat.showStats,
              action: () => {
                pairStore.settings.remoteCat.showStats = !pairStore.settings.remoteCat.showStats
              },
            }),
            CheckMenuItem.new({
              text: t('pages.preference.pair.labels.syncModel'),
              checked: pairStore.settings.remoteCat.syncModel !== false,
              action: () => {
                pairStore.settings.remoteCat.syncModel = pairStore.settings.remoteCat.syncModel === false
              },
            }),
          ]),
      PredefinedMenuItem.new({ item: 'Separator' }),
      CheckMenuItem.new({
        text: t('pages.preference.pair.labels.alwaysOnTop'),
        checked: windowSettings(target).alwaysOnTop,
        action: () => {
          windowSettings(target).alwaysOnTop = !windowSettings(target).alwaysOnTop
        },
      }),
      CheckMenuItem.new({
        text: t('composables.useAppMenu.labels.passThrough'),
        checked: windowSettings(target).passThrough,
        action: () => {
          windowSettings(target).passThrough = !windowSettings(target).passThrough
        },
      }),
      Submenu.new({
        text: t('composables.useAppMenu.labels.windowSize'),
        items: await getScaleMenuItems(target),
      }),
      Submenu.new({
        text: t('composables.useAppMenu.labels.opacity'),
        items: await getOpacityMenuItems(target),
      }),
    ])
  }

  const getExitMenu = async () => {
    return await Promise.all([
      MenuItem.new({
        id: 'bongo-restart-native',
        text: t('composables.useAppMenu.labels.restartApp'),
      }),
      MenuItem.new({
        id: 'bongo-exit-native',
        text: t('composables.useAppMenu.labels.quitApp'),
        accelerator: isMac ? 'Cmd+Q' : '',
      }),
    ])
  }

  return {
    getBaseMenu,
    getExitMenu,
  }
}

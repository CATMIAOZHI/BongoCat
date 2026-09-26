import { error } from '@tauri-apps/plugin-log'

import type { WindowLabel } from '@/plugins/window'

import { WINDOW_LABEL } from '@/constants'
import { hideWindowByLabel, showWindowByLabel } from '@/plugins/window'
import { usePairStore } from '@/stores/pair'

/**
 * 对方猫 / 聊天窗口的显示开关。
 *
 * 这两个浮层窗口的显示与否由 pair store 里的开关决定，但**不能只让目标窗口自己的页面
 * 去执行**：页面一旦被 Chromium 冻结（画面停在最后一帧、点不动、连自己的显示开关都
 * 失效，对方猫窗口出现过这个状态），它就既藏不掉自己也显示不出来。所以由「改开关的
 * 那个窗口」直接执行一次显示 / 隐藏——活着的窗口照样能把浮层切过来，冻结的页面不再
 * 挡路。两个窗口的页面各自还留着自己的 watcher 作为兜底。
 */
export function setRemoteCatVisible(visible: boolean) {
  usePairStore().settings.remoteCat.visible = visible

  applyVisibility(WINDOW_LABEL.REMOTE_CAT, visible)
}

export function setChatVisible(visible: boolean) {
  usePairStore().settings.chat.visible = visible

  applyVisibility(WINDOW_LABEL.CHAT, visible)
}

function applyVisibility(label: WindowLabel, visible: boolean) {
  const action = visible
    ? showWindowByLabel(label)
    : hideWindowByLabel(label)

  action.catch((reason) => {
    error(String(reason))
  })
}

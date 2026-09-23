import { invoke } from '@tauri-apps/api/core'
import { emit } from '@tauri-apps/api/event'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

import type { WINDOW_LABEL } from '../constants'

import { LISTEN_KEY } from '../constants'

export type WindowLabel = typeof WINDOW_LABEL[keyof typeof WINDOW_LABEL]

/**
 * 广播窗口显示状态变化。
 *
 * 用应用级事件而不是模块内订阅：托盘菜单在偏好窗口里构建，而切换动作可能来自
 * 猫咪窗口的右键菜单，模块内的订阅集合不会跨 WebView 生效。
 */
function notifyWindowVisibilityChange() {
  return emit(LISTEN_KEY.WINDOW_VISIBILITY_CHANGED)
}

const COMMAND = {
  SHOW_WINDOW: 'plugin:custom-window|show_window',
  HIDE_WINDOW: 'plugin:custom-window|hide_window',
  SET_ALWAYS_ON_TOP: 'plugin:custom-window|set_always_on_top',
  SET_TASKBAR_VISIBILITY: 'plugin:custom-window|set_taskbar_visibility',
  SHOW_WINDOW_LABEL: 'plugin:custom-window|show_window_label',
  HIDE_WINDOW_LABEL: 'plugin:custom-window|hide_window_label',
  IS_WINDOW_VISIBLE: 'plugin:custom-window|is_window_visible',
}

export function showWindow(label?: WindowLabel) {
  if (label) {
    emit(LISTEN_KEY.SHOW_WINDOW, label)
  } else {
    invoke(COMMAND.SHOW_WINDOW)
  }
}

export function hideWindow(label?: WindowLabel) {
  if (label) {
    emit(LISTEN_KEY.HIDE_WINDOW, label)
  } else {
    invoke(COMMAND.HIDE_WINDOW)
  }
}

/** 按 label 显示指定窗口，由任意窗口调用（不依赖目标窗口的 JS 是否就绪） */
export async function showWindowByLabel(label: WindowLabel, focus = false) {
  await invoke(COMMAND.SHOW_WINDOW_LABEL, { label, focus })

  await notifyWindowVisibilityChange()
}

/** 按 label 隐藏指定窗口 */
export async function hideWindowByLabel(label: WindowLabel) {
  await invoke(COMMAND.HIDE_WINDOW_LABEL, { label })

  await notifyWindowVisibilityChange()
}

/** 查询指定窗口当前是否可见 */
export function isWindowVisible(label: WindowLabel) {
  return invoke<boolean>(COMMAND.IS_WINDOW_VISIBLE, { label })
}

/** 切换指定窗口的显示状态，返回切换后的可见状态 */
export async function toggleWindowVisibleByLabel(label: WindowLabel, focus = false) {
  const visible = await isWindowVisible(label)

  if (visible) {
    await hideWindowByLabel(label)
  } else {
    await showWindowByLabel(label, focus)
  }

  return !visible
}

export function setAlwaysOnTop(alwaysOnTop: boolean) {
  invoke(COMMAND.SET_ALWAYS_ON_TOP, { alwaysOnTop })
}

export async function toggleWindowVisible(label?: WindowLabel) {
  const appWindow = getCurrentWebviewWindow()

  if (appWindow.label !== label) return

  const visible = await appWindow.isVisible()

  if (visible) {
    return hideWindow(label)
  }

  return showWindow(label)
}

export async function setTaskbarVisibility(visible: boolean) {
  invoke(COMMAND.SET_TASKBAR_VISIBILITY, { visible })
}

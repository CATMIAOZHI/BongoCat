export const GITHUB_LINK = 'https://github.com/ayangweb/BongoCat'

export const UPGRADE_LINK_ACCESS_KEY = 'xDbrq2rOoRThDqKOHL2ZRA'

export const LISTEN_KEY = {
  SHOW_WINDOW: 'show-window',
  HIDE_WINDOW: 'hide-window',
  DEVICE_CHANGED: 'device-changed',
  UPDATE_APP: 'update-app',
  GAMEPAD_CHANGED: 'gamepad-changed',
  START_MOTION: 'start-motion',
  SET_EXPRESSION: 'set-expression',
  WINDOW_VISIBILITY_CHANGED: 'window-visibility-changed',
  PAIR_CONNECTION_CHANGED: 'pair-connection-changed',
  PAIR_PEER_CHANGED: 'pair-peer-changed',
  PAIR_PRESENCE: 'pair-presence',
  PAIR_PET_STATE: 'pair-pet-state',
  PAIR_STATS: 'pair-stats',
  PAIR_MESSAGE: 'pair-message',
  PAIR_MESSAGE_RECEIVED: 'pair-message-received',
  PAIR_MESSAGE_UPDATED: 'pair-message-updated',
  PAIR_ERROR: 'pair-error',
  /** 前端之间的应用级事件：偏好窗口按下「聊天输入」快捷键后通知聊天窗口 */
  CHAT_INPUT_TOGGLE: 'chat-input-toggle',
  /** 前端之间的应用级事件：开始了新的记录周期，聊天窗口要重新读一遍历史 */
  CHAT_HISTORY_RESET: 'chat-history-reset',
}

export const INVOKE_KEY = {
  COPY_DIR: 'copy_dir',
  START_DEVICE_LISTENING: 'start_device_listening',
  START_GAMEPAD_LISTING: 'start_gamepad_listing',
  STOP_GAMEPAD_LISTING: 'stop_gamepad_listing',
  PAIR_GET_STATUS: 'pair_get_status',
  PAIR_GET_DEVICE_ID: 'pair_get_device_id',
  PAIR_SET_SECRET: 'pair_set_secret',
  PAIR_HAS_SECRET: 'pair_has_secret',
  PAIR_GET_SECRET_FINGERPRINT: 'pair_get_secret_fingerprint',
  PAIR_DELETE_SECRET: 'pair_delete_secret',
  PAIR_CONNECT: 'pair_connect',
  PAIR_DISCONNECT: 'pair_disconnect',
  PAIR_SEND_PRESENCE: 'pair_send_presence',
  PAIR_SEND_PET_STATE: 'pair_send_pet_state',
  PAIR_SEND_STATS: 'pair_send_stats',
  PAIR_SEND_CHAT: 'pair_send_chat',
  PAIR_HISTORY_LIST: 'pair_history_list',
  PAIR_HISTORY_STATS: 'pair_history_stats',
  PAIR_HISTORY_EXPORT: 'pair_history_export',
  PAIR_HISTORY_START_NEW_EPOCH: 'pair_history_start_new_epoch',
}

export const LANGUAGE = {
  ZH_CN: 'zh-CN',
  ZH_TW: 'zh-TW',
  EN_US: 'en-US',
  VI_VN: 'vi-VN',
  PT_BR: 'pt-BR',
} as const

export const WINDOW_LABEL = {
  MAIN: 'main',
  PREFERENCE: 'preference',
  REMOTE_CAT: 'remote-cat',
  CHAT: 'chat',
} as const

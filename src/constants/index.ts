export const GITHUB_LINK = 'https://github.com/ayangweb/BongoCat'

export const UPGRADE_LINK_ACCESS_KEY = 'xDbrq2rOoRThDqKOHL2ZRA'

/**
 * 聊天浮层（R39）占猫咪窗口的高度比例：**相对模型高度**。
 *
 * 浮层挂在猫咪窗口的上方一条里，猫咪本体贴底不动，所以窗口总高 = 模型高 × (1 + 这个值)。
 * 只有开启双人联机时窗口才会多出这一条（见 `pages/main/index.vue`）。
 * 鼠标悬停不淡出、以及「点不到」的分界线都用它算，所以放在常量里让几处共用同一份口径。
 */
export const CHAT_OVERLAY_RATIO = 0.62

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
  PAIR_TRANSFER: 'pair-transfer',
  PAIR_ERROR: 'pair-error',
  /** 前端之间的应用级事件：偏好窗口按下「聊天输入」快捷键后通知聊天窗口 */
  CHAT_INPUT_TOGGLE: 'chat-input-toggle',
  /** 前端之间的应用级事件：开始了新的记录周期，聊天窗口要重新读一遍历史 */
  CHAT_HISTORY_RESET: 'chat-history-reset',
}

export const INVOKE_KEY = {
  COPY_DIR: 'copy_dir',
  START_DEVICE_LISTENING: 'start_device_listening',
  IS_KEY_DOWN: 'is_key_down',
  START_GAMEPAD_LISTING: 'start_gamepad_listing',
  STOP_GAMEPAD_LISTING: 'stop_gamepad_listing',
  PAIR_GET_STATUS: 'pair_get_status',
  PAIR_GET_DEVICE_ID: 'pair_get_device_id',
  PAIR_SET_SECRET: 'pair_set_secret',
  PAIR_HAS_SECRET: 'pair_has_secret',
  PAIR_GENERATE_SECRET: 'pair_generate_secret',
  PAIR_GET_SECRET_FINGERPRINT: 'pair_get_secret_fingerprint',
  PAIR_DELETE_SECRET: 'pair_delete_secret',
  PAIR_GET_SECRET: 'pair_get_secret',
  PAIR_SET_SERVER_PASSWORD: 'pair_set_server_password',
  PAIR_HAS_SERVER_PASSWORD: 'pair_has_server_password',
  PAIR_GET_SERVER_PASSWORD: 'pair_get_server_password',
  PAIR_DELETE_SERVER_PASSWORD: 'pair_delete_server_password',
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
  PAIR_SET_MAX_ATTACHMENT_MB: 'pair_set_max_attachment_mb',
  PAIR_TRANSFER_ACCEPT: 'pair_transfer_accept',
  PAIR_TRANSFER_REJECT: 'pair_transfer_reject',
  PAIR_TRANSFER_CANCEL: 'pair_transfer_cancel',
  PAIR_ATTACHMENT_RETRY: 'pair_attachment_retry',
  PAIR_START_RECORDING: 'pair_start_recording',
  PAIR_STOP_RECORDING: 'pair_stop_recording',
  PAIR_SEND_RECORDING: 'pair_send_recording',
  PAIR_CANCEL_RECORDING: 'pair_cancel_recording',
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

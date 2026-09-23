<script setup lang="ts">
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

const appWindow = getCurrentWebviewWindow()

function handleMouseDown() {
  appWindow.startDragging()
}
</script>

<template>
  <!-- 桌面聊天气泡窗口：Phase 4 会渲染聊天内容，当前仅用于验证多窗口基础设施 -->
  <div class="size-screen flex flex-col bg-black/45 p-3 rounded-2xl">
    <div
      class="text-[12px] text-white/70"
      @mousedown="handleMouseDown"
    >
      Chat
    </div>
  </div>
</template>

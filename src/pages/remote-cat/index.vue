<script setup lang="ts">
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

const appWindow = getCurrentWebviewWindow()

function handleMouseDown() {
  appWindow.startDragging()
}
</script>

<template>
  <!-- 对方猫咪窗口：Phase 3 会渲染远程桌宠，当前仅用于验证多窗口基础设施 -->
  <div
    class="size-screen flex items-center justify-center"
    @mousedown="handleMouseDown"
  >
    <div class="bg-black/50 px-4 py-3 rounded-2xl">
      <span class="text-center text-[13px] text-white/85">Remote Cat</span>
    </div>
  </div>
</template>

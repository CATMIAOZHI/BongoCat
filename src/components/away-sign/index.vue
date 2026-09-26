<script setup lang="ts">
defineProps<{ text: string }>()
</script>

<template>
  <div
    :aria-label="text"
    class="away-sign"
    role="status"
    :title="text"
  >
    <div class="sign-board">
      <span
        aria-hidden="true"
        class="sign-sparkle"
      >✦</span>
      <span class="sign-text">{{ text }}</span>
      <span
        aria-hidden="true"
        class="sign-heart"
      >♡</span>
    </div>
    <span
      aria-hidden="true"
      class="sign-paw paw-left"
    ><i /></span>
    <span
      aria-hidden="true"
      class="sign-paw paw-right"
    ><i /></span>
    <span
      aria-hidden="true"
      class="sign-stick"
    />
  </div>
</template>

<style scoped>
.away-sign {
  position: absolute;
  bottom: 9%;
  left: 50%;
  width: min(68%, 220px);
  transform: translateX(-50%) rotate(-3deg);
  color: #695046;
  /* 保留完整文字的悬停提示；不截断鼠标事件，拖拽仍冒泡给猫窗口。 */
  pointer-events: auto;
  filter: drop-shadow(0 3px 3px #39271925);
}
.sign-board {
  position: relative;
  z-index: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  min-height: 42px;
  padding: 9px 16px 12px;
  border: 2px solid #bc9375;
  border-radius: 12px;
  background: #fff3da;
  box-shadow: inset 0 0 0 3px #fffaf0;
}
.sign-text {
  min-width: 0;
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 3;
  overflow: hidden;
  overflow-wrap: anywhere;
  text-align: center;
  font-size: clamp(10px, 3.6vw, 14px);
  font-weight: 600;
  line-height: 1.45;
}
.sign-heart,
.sign-sparkle {
  color: #c47c80;
  flex-shrink: 0;
  font-size: 14px;
}
.sign-stick {
  position: absolute;
  width: 10px;
  height: 19px;
  left: calc(50% - 5px);
  bottom: -15px;
  border: 2px solid #bc9375;
  background: #dfb48b;
  border-radius: 0 0 4px 4px;
}
.sign-paw {
  position: absolute;
  z-index: 2;
  bottom: -6px;
  width: 25px;
  height: 19px;
  border: 2px solid #bc9375;
  border-radius: 45% 45% 50% 50%;
  background: #fffaf3;
}
.paw-left {
  left: 11px;
  transform: rotate(12deg);
}
.paw-right {
  right: 11px;
  transform: rotate(-12deg);
}
.sign-paw i {
  position: absolute;
  width: 9px;
  height: 7px;
  border-radius: 50%;
  background: #eba6ad;
  left: 6px;
  top: 6px;
}
@media (max-width: 180px) {
  .sign-board {
    padding: 5px 8px 9px;
    min-height: 30px;
  }
  .sign-heart,
  .sign-sparkle {
    display: none;
  }
}
</style>

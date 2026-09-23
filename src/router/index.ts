import type { RouteRecordRaw } from 'vue-router'

import { createRouter, createWebHashHistory } from 'vue-router'

// 每个窗口都是独立的 WebView；按路由懒加载可让聊天窗口不必加载 Live2D / pixi.js
const routes: Readonly<RouteRecordRaw[]> = [
  {
    path: '/',
    component: () => import('../pages/main/index.vue'),
  },
  {
    path: '/preference',
    component: () => import('../pages/preference/index.vue'),
  },
  {
    path: '/remote-cat',
    component: () => import('../pages/remote-cat/index.vue'),
  },
  {
    path: '/chat',
    component: () => import('../pages/chat/index.vue'),
  },
]

const router = createRouter({
  history: createWebHashHistory(),
  routes,
})

export default router

import vue from '@vitejs/plugin-vue'
import { resolve } from 'node:path'
import { env } from 'node:process'
import UnoCSS from 'unocss/vite'
import { defineConfig } from 'vite'
import vitePluginDayjs from 'vite-plugin-dayjs'

const host = env.TAURI_DEV_HOST

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [vue(), UnoCSS(), vitePluginDayjs()],
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
    },
  },
  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: 'ws',
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri` and Rust build output
      // `target/` 在仓库根目录（Cargo workspace），Rust 重编会替换里面的 dll，
      // 不忽略的话 vite 的 watcher 会 EBUSY 崩掉整个 `tauri dev`
      ignored: ['**/src-tauri/**', '**/target/**'],
    },
  },
}))

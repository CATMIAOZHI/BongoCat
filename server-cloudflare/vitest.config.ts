import { cloudflareTest } from '@cloudflare/vitest-pool-workers'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: './wrangler.jsonc' },
      miniflare: {
        // 测试用固定 token；真实部署用 `wrangler secret put PAIR_AUTH_TOKEN`
        bindings: { PAIR_AUTH_TOKEN: 'test-token' },
      },
    }),
  ],
  test: {
    include: ['test/**/*.spec.ts'],
  },
})

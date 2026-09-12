import { defineConfig } from '@playwright/test'
import config from './transcript.playwright'
export default defineConfig({
  ...config,
  webServer: {
    ...config.webServer,
    command: 'npm run preview -- --port 41874 --strictPort',
    url: 'http://127.0.0.1:41874',
    reuseExistingServer: false,
  },
  testDir: '../../e2e',
  testMatch: [
    'session-detail.spec.ts',
    'product-session-send.spec.ts',
    'product-catalog.spec.ts',
    'product-shell.spec.ts',
    'product-evidence.spec.ts',
  ],
})

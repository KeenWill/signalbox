import { defineConfig } from '@playwright/test'
import config from '../../playwright.config'

export default defineConfig({
  ...config,
  use: { ...config.use, baseURL: 'http://127.0.0.1:41874' },
  testDir: '.',
  testMatch: '*.browser.ts',
  webServer: {
    command: 'npm run dev -- --port 41874 --strictPort',
    url: 'http://127.0.0.1:41874',
    reuseExistingServer: false,
  },
})

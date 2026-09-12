import { defineConfig } from '@playwright/test'
import base from '../../playwright.config'

export default defineConfig({
  ...base,
  testDir: '.',
  testMatch: '**/*.browser.ts',
  outputDir: '../../test-results/usage',
  use: { ...base.use, baseURL: 'http://127.0.0.1:4177' },
  webServer: {
    command: 'npm run dev -- --port 4177',
    url: 'http://127.0.0.1:4177',
    reuseExistingServer: !process.env.CI,
  },
})

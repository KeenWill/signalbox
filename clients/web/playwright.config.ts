import { defineConfig, devices } from '@playwright/test'

// Tunable effective ceiling: absorb cross-host text rasterization drift (observed at 2.2%) while
// preserving sensitivity to geometry and content regressions.
const CROSS_HOST_TEXT_RASTERIZATION_TOLERANCE = 0.035
// Tunable effective ceiling: two CI retries expose persistent browser failures without allowing
// flakes to consume unbounded matrix time or pass after repeated attempts.
const CI_BROWSER_RETRY_CEILING = 2
const WEB_TEST_PORT = process.env.SIGNALBOX_WEB_TEST_PORT ?? '4173'

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? CI_BROWSER_RETRY_CEILING : 0,
  reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : 'list',
  expect: {
    toHaveScreenshot: {
      maxDiffPixelRatio: CROSS_HOST_TEXT_RASTERIZATION_TOLERANCE,
    },
  },
  use: {
    baseURL: `http://127.0.0.1:${WEB_TEST_PORT}`,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  webServer: {
    command: `npm run preview -- --port ${WEB_TEST_PORT}`,
    url: `http://127.0.0.1:${WEB_TEST_PORT}`,
    reuseExistingServer: !process.env.CI,
  },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
    { name: 'firefox', use: { ...devices['Desktop Firefox'] } },
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
  ],
})

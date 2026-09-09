import { readFileSync } from 'node:fs'

import { defaultBrowserPreferences } from '../src/preferences'
import { expect, type Page, type TestInfo, test } from './fontTest'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const previewPath =
  '/api/blobs/sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e/content/image-png'
const originalPath =
  '/api/blobs/sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e/content/image-png'
const documentDownloadPath = `/api/blobs/sha256:${'6f'.repeat(32)}/download`
const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))

const watchBrowser = (page: Page): BrowserProblems => {
  const problems: BrowserProblems = { consoleErrors: [], pageErrors: [] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test.beforeEach(async ({ page }) => {
  await page.route(`**${previewPath}`, (route) =>
    route.fulfill({ body: previewFixture, contentType: 'image/png' }),
  )
})

for (const width of [790, 1440]) {
  test(`stacks attachments within the available pane at viewport ${width}`, async ({ page }) => {
    const problems = watchBrowser(page)
    await page.addInitScript(
      (preferences) => {
        localStorage.setItem('signalbox.web.preferences.v1', JSON.stringify(preferences))
      },
      { ...defaultBrowserPreferences, paneSizes: { navigation: 360, inspector: 480 } },
    )
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/scenario/attachments')
    const layout = page.locator('.attachment-layout')
    if (width === 790) {
      expect(await layout.evaluate((element) => element.clientWidth)).toBeLessThan(450)
    }
    await expect(layout).toHaveCSS('display', 'block')
    expect(await layout.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
      true,
    )
    const download = page.getByRole('link', { name: 'Download', exact: true })
    await download.scrollIntoViewIfNeeded()
    await expect(download).toBeInViewport()
    await page.screenshot({ path: test.info().outputPath('attachment-pane.png') })
    expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
  })
}

test('keeps document bytes behind the admitted download-only affordance', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/attachments')

  const preview = page.getByRole('region', { name: 'Selected attachment preview' })
  await expect(preview.getByRole('link', { name: 'Download', exact: true })).toBeVisible()
  await expect(preview.getByRole('link', { name: 'Open document' })).toHaveCount(0)
  const download = preview.getByRole('link', { name: 'Download' })
  await expect(download).toHaveAttribute('download')
  await expect(download).toHaveAttribute('href', new RegExp(`^${documentDownloadPath}`))
  expect(
    await page.evaluate(
      (path) =>
        performance
          .getEntriesByType('resource')
          .filter((entry) => new URL(entry.name).pathname === path).length,
      documentDownloadPath,
    ),
  ).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('selects an admitted derivative without loading original bytes', async ({ page }) => {
  const problems = watchBrowser(page)
  const previewResponse = page.waitForResponse(
    (response) => new URL(response.url()).pathname === previewPath,
  )
  await page.goto('/scenario/attachments')

  const derivative = page
    .getByRole('region', { name: 'Transcript attachments' })
    .getByRole('button', { name: /orbital-map\.preview\.png/ })
  await derivative.focus()
  await page.keyboard.press('Enter')
  await expect(derivative).toHaveAttribute('aria-pressed', 'true')
  await expect(
    page.getByRole('img', { name: 'Derived preview of orbital-map.preview.png' }),
  ).toBeVisible()
  await previewResponse
  await expect(page.getByText('image.preview v1')).toBeVisible()
  expect(
    await page.evaluate(
      (path) => performance.getEntriesByName(new URL(path, location.href).href).length,
      originalPath,
    ),
  ).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('fails closed when an admitted derivative cannot load', async ({ page }) => {
  await page.route(`**${previewPath}`, (route) => route.fulfill({ status: 404 }))
  await page.goto('/scenario/attachments')

  await page
    .getByRole('region', { name: 'Transcript attachments' })
    .getByRole('button', { name: /orbital-map\.preview\.png/ })
    .click()

  await expect(page.getByRole('status').getByText('Derivative unavailable')).toBeVisible()
  await expect(
    page.getByRole('img', { name: 'Derived preview of orbital-map.preview.png' }),
  ).toHaveCount(0)
})

test('renders media placeholders and removes a composer attachment by keyboard', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/attachments')

  const transcript = page.getByRole('region', { name: 'Transcript attachments' })
  const audio = transcript.getByRole('button', { name: /operator-note\.ogg/ })
  await audio.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByText('Audio playback unavailable')).toBeVisible()
  await expect(page.locator('audio, video')).toHaveCount(0)

  const composer = page.getByRole('region', { name: 'Composer attachments' })
  const remove = composer.getByRole('button', { name: 'Remove architecture.pdf' })
  await remove.focus()
  await page.keyboard.press('Enter')
  await expect(composer.getByText('architecture.pdf')).toHaveCount(0)
  await expect(transcript.getByText('architecture.pdf')).toBeVisible()
  await expect(composer.locator('.attachment-select').first()).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop dark attachment evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/attachments')
  await expect(page.getByRole('region', { name: 'Artifact attachments' })).toHaveScreenshot(
    'attachments-desktop-dark.png',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop light attachment evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/attachments')
  await page.getByRole('button', { name: 'Use light theme' }).focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.getByRole('region', { name: 'Artifact attachments' })).toHaveScreenshot(
    'attachments-desktop-light.png',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures mobile attachment evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/attachments')
  await expect(page.getByRole('region', { name: 'Artifact attachments' })).toHaveScreenshot(
    'attachments-mobile-dark.png',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

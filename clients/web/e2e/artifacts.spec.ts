import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

import { expect, type Page, type TestInfo, test } from '@playwright/test'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const previewPath =
  '/api/blobs/sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e/content/image-png'
const originalPath =
  '/api/blobs/sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e/content/image-png'
const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const originalFixture = readFileSync(new URL('./fixtures/original.png', import.meta.url))
const binaryDownloadPath = `/api/blobs/sha256:${'3c'.repeat(32)}/download`

test('fixture bytes match their advertised immutable identities', () => {
  expect(createHash('sha256').update(originalFixture).digest('hex')).toBe(
    '3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e',
  )
  expect(originalFixture.byteLength).toBe(33749)
  expect(createHash('sha256').update(previewFixture).digest('hex')).toBe(
    '071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e',
  )
  expect(previewFixture.byteLength).toBe(215370)
})

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
  await page.route('**/api/blobs/**/content/image-png', async (route) => {
    const path = new URL(route.request().url()).pathname
    const body = path === originalPath ? originalFixture : previewFixture
    await route.fulfill({ body, contentType: 'image/png' })
  })
})

test.afterEach(async ({ page }, testInfo) => {
  const diagnostics = await page
    .evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
    .catch(() => undefined)
  await testInfo.attach('signalbox-diagnostics', {
    body: JSON.stringify(diagnostics ?? null, null, 2),
    contentType: 'application/json',
  })
})

test('selects a bounded image capability and admits a bounded original explicitly', async ({
  page,
}) => {
  const problems = watchBrowser(page)

  const previewResponse = page.waitForResponse(
    (response) => new URL(response.url()).pathname === previewPath,
  )
  await page.goto('/scenario/blobs')
  const image = page.getByRole('img', { name: 'Preview of orbital-map.png' })
  await expect(image).toHaveAttribute('src', previewPath)
  await expect(image).toBeVisible()
  await expect
    .poll(() => image.evaluate((element) => (element as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
  expect((await previewResponse).headers()['content-type']).toContain('image/png')
  expect(
    await page.evaluate(
      (path) => performance.getEntriesByName(new URL(path, location.href).href).length,
      originalPath,
    ),
  ).toBe(0)

  const originalResponse = page.waitForResponse(
    (response) => new URL(response.url()).pathname === originalPath,
  )
  await page.getByRole('button', { name: 'Load original' }).click()
  await expect(page.getByRole('button', { name: 'Original loaded' })).toBeFocused()
  const original = page.getByRole('img', { name: 'Original of orbital-map.png' })
  await expect(original).toBeVisible()
  await expect(original).toHaveAttribute('src', originalPath)
  await expect
    .poll(() => original.evaluate((element) => (element as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
  expect((await originalResponse).headers()['content-type']).toContain('image/png')
  await page.keyboard.press('Escape')
  await expect(page.getByRole('region', { name: 'Blob evidence' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('falls back to metadata and download for an unknown binary capability', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/blobs/**/download?*', async (route) => {
    await route.fulfill({ body: 'fixture', contentType: 'application/octet-stream' })
  })
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact telemetry.capture' })
  await expect(artifact.getByText('metadata fallback')).toBeVisible()
  const download = artifact.getByRole('link', { name: 'Download' })
  await expect(download).toHaveAttribute('download', 'telemetry.capture')
  const href = await download.getAttribute('href')
  expect(href).not.toBeNull()
  const url = new URL(href ?? '', page.url())
  expect(url.origin).toBe(new URL(page.url()).origin)
  expect(url.pathname).toBe(binaryDownloadPath)
  expect(url.searchParams.get('media_type')).toBe('application/octet-stream')
  expect(url.searchParams.get('display_filename')).toBe('telemetry.capture')
  const browserDownload = page.waitForEvent('download')
  await download.click()
  expect((await browserDownload).suggestedFilename()).toBe('telemetry.capture')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the capability-driven artifact workbench', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')
  await expect(page.getByRole('heading', { name: 'Blob evidence' })).toBeVisible()
  await expect(page).toHaveScreenshot('artifacts-dark.png', { fullPage: true })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

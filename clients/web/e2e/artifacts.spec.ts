import { readFileSync } from 'node:fs'

import { expect, type Page, type TestInfo, test } from '@playwright/test'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const previewPath = `/api/blobs/sha256:${'2b'.repeat(32)}/content/image-png`
const originalPath = `/api/blobs/sha256:${'1a'.repeat(32)}/content/image-png`
const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const originalFixture = readFileSync(new URL('./fixtures/original.png', import.meta.url))

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

test('selects an image capability without prefetching original bytes', async ({ page }) => {
  const problems = watchBrowser(page)
  const blobRequests: string[] = []
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname
    if (path.startsWith('/api/blobs/')) blobRequests.push(path)
  })

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
  expect(blobRequests).toContain(previewPath)
  expect(blobRequests).not.toContain(originalPath)

  const originalResponse = page.waitForResponse(
    (response) => new URL(response.url()).pathname === originalPath,
  )
  await page.getByRole('button', { name: 'Load original' }).click()
  await expect(image).toHaveAttribute('src', originalPath)
  await expect
    .poll(() => image.evaluate((element) => (element as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
  expect((await originalResponse).headers()['content-type']).toContain('image/png')
  expect(blobRequests).toContain(originalPath)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('falls back to metadata and download for an unknown binary capability', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact telemetry.capture' })
  await expect(artifact.getByText('metadata fallback')).toBeVisible()
  await expect(artifact.getByRole('link', { name: 'Download' })).toHaveAttribute(
    'download',
    'telemetry.capture',
  )
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

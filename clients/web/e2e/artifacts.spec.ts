import { readFileSync } from 'node:fs'

import { expect, type Page, type TestInfo, test } from '@playwright/test'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const previewPath = `/api/blobs/sha256:${'2b'.repeat(32)}/content/image-png`
const originalPath = `/api/blobs/sha256:${'1a'.repeat(32)}/content/image-png`
const remotePath = 'https://media.example.test/remote-status-diagram.png'
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

test('selects admitted image views without prefetching original bytes', async ({ page }) => {
  const problems = watchBrowser(page)

  const previewResponse = page.waitForResponse(
    (response) => new URL(response.url()).pathname === previewPath,
  )
  await page.goto('/scenario/blobs')
  const image = page.getByRole('img', { name: 'Preview of orbital-map.png' })
  await expect(image).toBeVisible()
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
  const loadOriginal = page.getByRole('button', { name: 'Load original' })
  await loadOriginal.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('img', { name: 'Original of orbital-map.png' })).toBeVisible()
  expect((await originalResponse).headers()['content-type']).toContain('image/png')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('expands text through a bounded keyboard action', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact incident-notes.txt' })
  const expand = artifact.getByRole('button', { name: 'Expand bounded preview' })
  await expand.focus()
  await expect(expand).toBeFocused()
  await page.keyboard.press('Enter')
  await expect(artifact.getByRole('button', { name: 'Collapse preview' })).toBeFocused()
  await expect(artifact.getByText(/characters remain outside this bounded view/)).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps remote media behind the persisted ask policy', async ({ page }) => {
  const problems = watchBrowser(page)
  let requests = 0
  await page.route(remotePath, async (route) => {
    requests += 1
    await route.fulfill({ body: previewFixture, contentType: 'image/png' })
  })
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact remote-status-diagram.png' })
  await expect(artifact.getByLabel('Remote media not loaded')).toBeVisible()
  expect(requests).toBe(0)
  const load = artifact.getByRole('button', { name: 'Load this remote image' })
  await load.focus()
  await page.keyboard.press('Enter')
  await expect(artifact.getByRole('img', { name: 'Remote status diagram' })).toBeVisible()
  expect(requests).toBe(1)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('blocks remote media without exposing a load action', async ({ page }) => {
  const problems = watchBrowser(page)
  let requests = 0
  await page.route(remotePath, async (route) => {
    requests += 1
    await route.fulfill({ body: previewFixture, contentType: 'image/png' })
  })
  await page.goto('/scenario/blobs')
  await page.getByRole('combobox', { name: 'Remote media' }).selectOption('block')

  const artifact = page.getByRole('article', { name: 'Artifact remote-status-diagram.png' })
  await expect(artifact.getByText('remote media block')).toBeVisible()
  await expect(artifact.getByRole('button', { name: 'Load this remote image' })).toHaveCount(0)
  expect(requests).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('renders unsupported and unauthorized kinds as typed safe states', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  const document = page.getByRole('article', { name: 'Artifact architecture.pdf' })
  await expect(document.getByText('Typed renderer not implemented')).toBeVisible()
  await expect(document.getByText('No bytes were read.')).toBeVisible()
  const blocked = page.getByRole('article', { name: 'Artifact restricted.capture' })
  await expect(blocked.getByText('Artifact blocked')).toBeVisible()
  await expect(blocked.getByRole('link')).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop dark artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/blobs')
  await expect(page.getByRole('heading', { name: 'Artifact renderers' })).toBeVisible()
  await expect(page).toHaveScreenshot('artifacts-desktop-dark.png', { fullPage: true })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop light artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/blobs')
  await page.getByRole('button', { name: 'Use light theme' }).focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page).toHaveScreenshot('artifacts-desktop-light.png', { fullPage: true })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures mobile artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/blobs')
  await expect(page.getByRole('heading', { name: 'Artifact renderers' })).toBeVisible()
  await expect(page).toHaveScreenshot('artifacts-mobile-dark.png', { fullPage: true })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

import { expect, type Page, type TestInfo, test } from '@playwright/test'
import {
  artifactScenario,
  fallbackDescriptor,
  imageDescriptor,
  imageOriginalView,
  imagePreviewView,
  imageThumbnailView,
  jpegDescriptor,
  jpegOriginalView,
} from '../src/features/artifacts/artifactScenario'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const previewPath = imagePreviewView.content_url
const originalPath = imageOriginalView.content_url
const thumbnailPath = imageThumbnailView.content_url
const jpegOriginalPath = jpegOriginalView.content_url
const remotePath = 'https://media.example.test/remote-status-diagram.png'
const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const originalFixture = readFileSync(new URL('./fixtures/original.png', import.meta.url))
const thumbnailFixture = readFileSync(new URL('./fixtures/thumbnail.png', import.meta.url))
const jpegOriginalFixture = readFileSync(new URL('./fixtures/original.jpg', import.meta.url))
const sha256Digest = (bytes: Buffer): string =>
  `sha256:${createHash('sha256').update(bytes).digest('hex')}`

const previewOutputDigest = imagePreviewView.derivations[0]?.output_digests[0]
const thumbnailOutputDigest = imageThumbnailView.derivations[0]?.output_digests[0]
if (previewOutputDigest === undefined || thumbnailOutputDigest === undefined) {
  throw new Error('the preview and thumbnail fixtures must advertise derivation output digests')
}

test('fixture bytes match their advertised immutable identities', () => {
  expect(sha256Digest(originalFixture)).toBe(imageDescriptor.digest)
  expect(String(originalFixture.byteLength)).toBe(imageOriginalView.byte_length)
  expect(sha256Digest(previewFixture)).toBe(previewOutputDigest)
  expect(String(previewFixture.byteLength)).toBe(imagePreviewView.byte_length)
  expect(sha256Digest(thumbnailFixture)).toBe(thumbnailOutputDigest)
  expect(String(thumbnailFixture.byteLength)).toBe(imageThumbnailView.byte_length)
  expect(sha256Digest(jpegOriginalFixture)).toBe(jpegDescriptor.digest)
  expect(String(jpegOriginalFixture.byteLength)).toBe(jpegOriginalView.byte_length)
})

const watchBrowser = (page: Page): BrowserProblems => {
  const problems: BrowserProblems = { consoleErrors: [], pageErrors: [] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const expectOnlyExpectedFailedResourceError = (
  problems: BrowserProblems,
  failedResponsePaths: readonly string[],
  expectedPaths: readonly string[],
) => {
  expect(problems.pageErrors).toEqual([])
  expect(failedResponsePaths).toEqual(expectedPaths)
  expect(
    problems.consoleErrors.filter(
      (message) =>
        !/^Failed to load resource: the server responded with a status of 500(?: |$)/u.test(
          message,
        ),
    ),
  ).toEqual([])
  // Chromium emits one generic failed-resource diagnostic; Firefox emits none. The exact failed
  // response assertion above correlates either behavior with only the intentional preview request.
  expect(problems.consoleErrors.length).toBeLessThanOrEqual(expectedPaths.length)
}

// Observe every HTTP error response the browser receives, rather than recording inside the
// intentional route: an unrelated failed response is then caught by the exact-path assertion even
// when the browser emits no console diagnostic for it.
const watchFailedResponses = (page: Page): string[] => {
  const failedResponsePaths: string[] = []
  page.on('response', (response) => {
    if (response.status() >= 400) failedResponsePaths.push(new URL(response.url()).pathname)
  })
  return failedResponsePaths
}

const failRouteOnce = async (page: Page, path: string): Promise<void> => {
  await page.route(
    `**${path}`,
    async (route) => {
      await route.fulfill({ status: 500, body: 'unavailable' })
    },
    { times: 1 },
  )
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
    const body =
      path === originalPath
        ? originalFixture
        : path === thumbnailPath
          ? thumbnailFixture
          : previewFixture
    await route.fulfill({ body, contentType: 'image/png' })
  })
  await page.route('**/api/blobs/**/content/image-jpeg', async (route) => {
    await route.fulfill({ body: jpegOriginalFixture, contentType: 'image/jpeg' })
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

test('selects a bounded image view and keeps an animation-capable original download-only', async ({
  page,
}) => {
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

  const artifact = page.getByRole('article', { name: 'Artifact orbital-map.png' })
  await expect(artifact.getByRole('button', { name: 'Load original' })).toHaveCount(0)
  expect(
    await page.evaluate(
      (path) => performance.getEntriesByName(new URL(path, location.href).href).length,
      originalPath,
    ),
  ).toBe(0)
  await expect(artifact.getByRole('link', { name: 'Download' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('advances from a failed preview to its admitted thumbnail', async ({ page }) => {
  const problems = watchBrowser(page)
  const failedResponsePaths = watchFailedResponses(page)
  await failRouteOnce(page, previewPath)
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact orbital-map.png' })
  await expect(artifact.getByRole('img', { name: 'Thumbnail of orbital-map.png' })).toBeVisible()
  await expect(artifact.getByText('thumbnail', { exact: true })).toBeVisible()
  await expect(artifact.getByRole('link', { name: 'Download' })).toBeVisible()
  expectOnlyExpectedFailedResourceError(problems, failedResponsePaths, [previewPath])
})

test('retries a bounded JPEG original and hides obsolete automatic failure status', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  const failedResponsePaths = watchFailedResponses(page)
  await failRouteOnce(page, thumbnailPath)
  await failRouteOnce(page, jpegOriginalPath)
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact bounded-photo.jpg' })
  await artifact.scrollIntoViewIfNeeded()
  await expect(artifact.getByRole('status')).toContainText(
    'No admitted inline image view could be loaded',
  )
  await artifact.getByRole('button', { name: 'Load original' }).click()
  await expect(artifact.getByRole('button', { name: 'Retry original' })).toBeVisible()
  await expect(artifact.getByRole('status')).toContainText('Original image failed to load')

  await artifact.getByRole('button', { name: 'Retry original' }).click()
  await expect(artifact.getByRole('img', { name: 'Original of bounded-photo.jpg' })).toBeVisible()
  await expect(artifact.getByRole('button', { name: 'Original loaded' })).toHaveAttribute(
    'aria-disabled',
    'true',
  )
  await expect(artifact.getByText('No admitted inline image view could be loaded')).toHaveCount(0)
  expectOnlyExpectedFailedResourceError(problems, failedResponsePaths, [
    thumbnailPath,
    jpegOriginalPath,
  ])
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
  await expect(artifact.getByText('Complete bounded content shown')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('selects artifacts independently and scopes preview commands', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  await page.getByRole('button', { name: /incident-notes\.txt/ }).click()
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await expect(page.getByRole('button', { name: /Expand bounded artifact preview/ })).toBeVisible()
  await page.keyboard.press('Escape')

  await page.getByRole('button', { name: /orbital-map\.png/ }).click()
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await expect(page.getByRole('button', { name: /Expand bounded artifact preview/ })).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keyboard-scrolls overflowing artifact content', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  const preview = page.getByRole('textbox', { name: 'Bounded preview of renderer.ts' })
  await preview.focus()
  await expect(preview).toBeFocused()
  await page.keyboard.press('PageDown')
  await expect.poll(() => preview.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns Escape focus to the artifact that owns the focused content', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  await page.getByRole('button', { name: /incident-notes\.txt/ }).click()
  const heading = page.getByRole('button', { name: /renderer\.ts/ })
  const preview = page.getByRole('textbox', { name: 'Bounded preview of renderer.ts' })
  await preview.focus()
  await expect(heading).toHaveAttribute('aria-pressed', 'true')
  await page.keyboard.press('Escape')
  await expect(heading).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps remote media unavailable without a bounded owning service', async ({ page }) => {
  const problems = watchBrowser(page)
  let requests = 0
  await page.route(remotePath, async (route) => {
    requests += 1
    await route.fulfill({ body: previewFixture, contentType: 'image/png' })
  })
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact remote-status-diagram.png' })
  await expect(artifact.getByLabel('Remote media not loaded')).toBeVisible()
  await expect(artifact.getByText('remote media unavailable')).toBeVisible()
  await expect(artifact.getByText('No bytes were fetched.')).toBeVisible()
  await expect(artifact.getByRole('button', { name: 'Load this remote image' })).toHaveCount(0)
  expect(requests).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps a generic descriptor available as metadata and download', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

  const artifact = page.getByRole('article', { name: 'Artifact trace.bin' })
  await expect(artifact.getByLabel('No compatible inline renderer')).toBeVisible()
  await expect(artifact.getByText('metadata fallback')).toBeVisible()
  await expect(artifact.getByText('application/octet-stream')).toBeVisible()
  await expect(
    artifact.getByText(`${BigInt(fallbackDescriptor.byte_length).toLocaleString()} bytes`),
  ).toBeVisible()
  await expect(artifact.getByRole('link', { name: 'Download' })).toHaveAttribute(
    'href',
    /display_filename=trace\.bin/,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('renders unauthorized kinds as typed safe states', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/blobs')

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
  // Pixel tolerances absorb small text drift, so the record count is pinned functionally: stale
  // whole-panel evidence cannot pass by tolerance alone when the scenario inventory changes.
  await expect(page.getByText(`${artifactScenario.length} typed records`)).toBeVisible()
  // TEMPORARY golden harvest: fail the stale baseline at zero tolerance so CI uploads the fresh
  // capture as evidence; reverted once the regenerated goldens are committed.
  await expect(page.getByRole('region', { name: 'Artifact renderers' })).toHaveScreenshot(
    'artifacts-desktop-dark.png',
    { maxDiffPixels: 0 },
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

const captureArtifactState = async (
  page: Page,
  testInfo: TestInfo,
  name: string,
  screenshot: string,
) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/blobs')
  const artifact = page.getByRole('article', { name })
  await artifact.scrollIntoViewIfNeeded()
  await expect(artifact).toHaveScreenshot(screenshot)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
}

test('captures the image renderer state', async ({ page }, testInfo) => {
  await captureArtifactState(page, testInfo, 'Artifact orbital-map.png', 'artifact-image-state.png')
})

test('captures the remote-unavailable renderer state', async ({ page }, testInfo) => {
  await captureArtifactState(
    page,
    testInfo,
    'Artifact remote-status-diagram.png',
    'artifact-remote-unavailable-state.png',
  )
})

test('captures the blocked renderer state', async ({ page }, testInfo) => {
  await captureArtifactState(
    page,
    testInfo,
    'Artifact restricted.capture',
    'artifact-blocked-state.png',
  )
})

test('captures desktop light artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.goto('/scenario/blobs')
  await page.getByRole('button', { name: 'Use light theme' }).focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.getByText(`${artifactScenario.length} typed records`)).toBeVisible()
  // TEMPORARY golden harvest: see the desktop-dark capture above.
  await expect(page.getByRole('region', { name: 'Artifact renderers' })).toHaveScreenshot(
    'artifacts-desktop-light.png',
    { maxDiffPixels: 0 },
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures mobile artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/blobs')
  await expect(page.getByRole('heading', { name: 'Artifact renderers' })).toBeVisible()
  await expect(page.getByText(`${artifactScenario.length} typed records`)).toBeVisible()
  // TEMPORARY golden harvest: see the desktop-dark capture above.
  await expect(page.getByRole('region', { name: 'Artifact renderers' })).toHaveScreenshot(
    'artifacts-mobile-dark.png',
    { maxDiffPixels: 0 },
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

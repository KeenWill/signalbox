import { readFileSync } from 'node:fs'
import { imageArtifact, jpegDescriptor } from '../src/features/artifacts/artifactScenario'
import { decodeWebBlobDescriptor } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, type Page, type TestInfo, test } from './fontTest'

const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const thumbnailFixture = readFileSync(new URL('./fixtures/thumbnail.png', import.meta.url))
const jpegOriginalFixture = readFileSync(new URL('./fixtures/original.jpg', import.meta.url))
const incompatibleDescriptorFixture = { invented: true } as const
const incompatibleDescriptorMessage = 'The server sent an unexpected response.'
// The shared renderer admits an inline original only for a single-frame JPEG carrying a bounded
// decode proof, so the inspector borrows the landed scenario descriptor that satisfies it.
const admittedOriginalArtifact = jpegDescriptor
const oversizedOriginalArtifact = decodeWebBlobDescriptor({
  ...jpegDescriptor,
  byte_length: '16777217',
  available_views: jpegDescriptor.available_views.map((view) =>
    view.kind === 'download' || view.kind === 'browser_native'
      ? { ...view, byte_length: '16777217' }
      : view,
  ),
})
const thumbnailContentPath = jpegDescriptor.available_views.find(
  (view) => view.kind === 'preview',
)?.content_url

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const platformModifier = (page: Page) =>
  page.evaluate(() => (/Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control'))

const useArtifactScenario = async (page: Page, descriptor = imageArtifact) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) => route.fulfill({ json: descriptor }))
  await page.route('**/api/blobs/**/content/image-png', (route) => {
    const pathname = new URL(route.request().url()).pathname
    const body = pathname === thumbnailContentPath ? thumbnailFixture : previewFixture
    return route.fulfill({ body, contentType: 'image/png' })
  })
  await page.route('**/api/blobs/**/content/image-jpeg', (route) =>
    route.fulfill({ body: jpegOriginalFixture, contentType: 'image/jpeg' }),
  )
}

const useRecoveringArtifactScenario = async (page: Page) => {
  const state = { unavailable: true }
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) => {
    if (state.unavailable) {
      return route.fulfill({ json: incompatibleDescriptorFixture })
    }
    return route.fulfill({ json: imageArtifact })
  })
  return { recover: () => (state.unavailable = false) }
}

const submitArtifactWithoutMouse = async (page: Page, descriptor = imageArtifact) => {
  const openInspector = page.getByRole('button', { name: 'Open artifact inspector' })
  await openInspector.focus()
  await page.keyboard.press('Enter')
  const digest = page.getByRole('textbox', { name: 'Digest' })
  await expect(digest).toBeFocused()
  await page.keyboard.type(descriptor.digest)
  await page.keyboard.press('Tab')
  await page.keyboard.type(descriptor.declared_media_type)
  await page.keyboard.press('Tab')
  await page.keyboard.type(descriptor.display_filename[0] ?? '')
  await page.keyboard.press('Tab')
  const descriptorRequest = page.waitForRequest('**/api/blobs/**/descriptor?*')
  await page.keyboard.press('Enter')
  const requestUrl = new URL((await descriptorRequest).url())
  expect(requestUrl.pathname).toBe(`/api/blobs/${encodeURIComponent(descriptor.digest)}/descriptor`)
  expect(requestUrl.searchParams.get('media_type')).toBe(descriptor.declared_media_type)
  expect(requestUrl.searchParams.get('display_filename')).toBe(descriptor.display_filename[0])
}

const resolveArtifactWithoutMouse = async (page: Page, descriptor = imageArtifact) => {
  const displayName = descriptor.display_filename[0]
  await submitArtifactWithoutMouse(page, descriptor)
  const artifact = page.getByRole('article', { name: `Artifact ${displayName}` })
  await expect(artifact).toBeVisible()
  await expect(page.getByText(`Found ${displayName}`, { exact: true })).toHaveAttribute(
    'role',
    'status',
  )
  const preview = artifact.getByRole('img', { name: `Preview of ${displayName}` })
  await expect(preview).toBeVisible()
  await expect
    .poll(() => preview.evaluate((element) => (element as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('resolves a typed artifact in the desktop side inspector without a mouse', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await resolveArtifactWithoutMouse(page)
  await page.keyboard.press('Escape')
  await expect(page.getByRole('complementary', { name: 'Inspector' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Open artifact inspector' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses a focus-managed artifact sheet on a phone viewport', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/sessions?workspace=true')

  await resolveArtifactWithoutMouse(page)
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeHidden()
  await expect(page.getByRole('button', { name: 'Open artifact inspector' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves an active artifact when the inspector changes composition', async ({ page }) => {
  const problems = watchBrowser(page)
  const displayName = admittedOriginalArtifact.display_filename[0]
  await useArtifactScenario(page, admittedOriginalArtifact)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await resolveArtifactWithoutMouse(page, admittedOriginalArtifact)
  await page.getByRole('button', { name: 'Load original' }).click()
  await expect(page.getByRole('button', { name: 'Original loaded' })).toBeVisible()
  await page.setViewportSize({ width: 1024, height: 900 })
  const sheet = page.getByRole('dialog', { name: 'Artifact inspector' })
  await expect(sheet.getByRole('textbox', { name: 'Digest' })).toHaveValue(
    admittedOriginalArtifact.digest,
  )
  await expect(sheet.getByRole('article', { name: `Artifact ${displayName}` })).toBeVisible()
  await expect(sheet.getByRole('img', { name: `Original of ${displayName}` })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves the side inspector beneath the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await resolveArtifactWithoutMouse(page)
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not restore stale side-inspector focus after closing a narrow sheet', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  await page.setViewportSize({ width: 1024, height: 900 })
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('button', { name: 'Open artifact inspector' })).toBeFocused()
  const theme = page.getByRole('button', { name: 'Use light theme' })
  await theme.focus()
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(theme).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores inspector focus when a sheet returns to the side pane', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  await page.setViewportSize({ width: 1024, height: 900 })
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeVisible()
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(page.getByRole('textbox', { name: 'Digest' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves editing context when Escape is pressed in an inspector input', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')

  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  const digest = page.getByRole('textbox', { name: 'Digest' })
  await digest.press('Escape')
  await expect(digest).toBeFocused()
  await expect(page.getByRole('heading', { name: 'Artifact inspector' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps an oversized browser-native original download-only', async ({ page }) => {
  const problems = watchBrowser(page)
  const displayName = oversizedOriginalArtifact.display_filename[0]
  await useArtifactScenario(page, oversizedOriginalArtifact)
  await page.goto('/sessions?workspace=true')

  await resolveArtifactWithoutMouse(page, oversizedOriginalArtifact)
  const artifact = page.getByRole('article', { name: `Artifact ${displayName}` })
  await expect(artifact.getByRole('button', { name: 'Load original' })).toHaveCount(0)
  await expect(artifact.getByRole('link', { name: 'Download' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('discovers the artifact inspector through the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.goto('/sessions?workspace=true')

  await expect(
    page.getByRole('button', { name: 'Open artifact inspector', exact: true }),
  ).toBeEnabled()
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toBeFocused()
  await page.getByRole('button', { name: 'Open artifact inspector', exact: true }).focus()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await palette.getByRole('button', { name: /Open artifact inspector/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('textbox', { name: 'Digest' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('recovers after a descriptor response violates the generated contract', async ({ page }) => {
  const problems = watchBrowser(page)
  const scenario = await useRecoveringArtifactScenario(page)
  await page.goto('/sessions?workspace=true')

  await submitArtifactWithoutMouse(page)
  await expect(page.getByRole('alert')).toContainText(incompatibleDescriptorMessage)
  scenario.recover()
  await page.getByRole('button', { name: 'Retry' }).focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Digest' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps focus moved during a pending descriptor retry', async ({ page }) => {
  const problems = watchBrowser(page)
  await useRecoveringArtifactScenario(page)
  await page.goto('/sessions?workspace=true')
  await submitArtifactWithoutMouse(page)
  await expect(page.getByRole('alert')).toContainText(incompatibleDescriptorMessage)

  const response = Promise.withResolvers<void>()
  await page.route('**/api/blobs/**/descriptor?*', async (route) => {
    await response.promise
    await route.fulfill({ json: imageArtifact })
  })
  const request = page.waitForRequest('**/api/blobs/**/descriptor?*')
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await request
  const filename = page.getByRole('textbox', { name: 'Display filename' })
  await filename.focus()
  response.resolve()
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  await expect(filename).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps focus moved during a pending bootstrap retry', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: incompatibleDescriptorFixture }),
  )
  await page.goto('/sessions?workspace=true')
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()

  const response = Promise.withResolvers<void>()
  await page.route('**/api/bootstrap', async (route) => {
    await response.promise
    await route.fulfill({ json: webContractBootstrapFixture })
  })
  const request = page.waitForRequest('**/api/bootstrap')
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await request
  const navigation = page.getByRole('link', { name: /Settings/ })
  await navigation.focus()
  response.resolve()
  await expect(page.getByText('Unexpected daemon response')).toHaveCount(0)
  await expect(navigation).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves an intentional blur during a pending descriptor retry', async ({ page }) => {
  const problems = watchBrowser(page)
  await useRecoveringArtifactScenario(page)
  await page.goto('/sessions?workspace=true')
  await submitArtifactWithoutMouse(page)
  await expect(page.getByRole('alert')).toContainText(incompatibleDescriptorMessage)

  const response = Promise.withResolvers<void>()
  await page.route('**/api/blobs/**/descriptor?*', async (route) => {
    await response.promise
    await route.fulfill({ json: imageArtifact })
  })
  const request = page.waitForRequest('**/api/blobs/**/descriptor?*')
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await request
  await page.getByText('Signalbox', { exact: true }).click()
  await expect(page.locator('body')).toBeFocused()
  response.resolve()
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  await expect(page.locator('body')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves an intentional blur during a pending bootstrap retry', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: incompatibleDescriptorFixture }),
  )
  await page.goto('/sessions?workspace=true')
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()

  const response = Promise.withResolvers<void>()
  await page.route('**/api/bootstrap', async (route) => {
    await response.promise
    await route.fulfill({ json: webContractBootstrapFixture })
  })
  const request = page.waitForRequest('**/api/bootstrap')
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await request
  await page.getByText('Signalbox', { exact: true }).click()
  await expect(page.locator('body')).toBeFocused()
  response.resolve()
  await expect(page.locator('.product-connection')).toHaveCount(0)
  await expect(page.locator('body')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop and responsive artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions?workspace=true')
  await resolveArtifactWithoutMouse(page)

  await expect.soft(page).toHaveScreenshot('artifact-inspector-desktop-dark.png', {
    animations: 'disabled',
  })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('artifact-inspector-desktop-light.png', {
    animations: 'disabled',
  })
  await page.getByRole('button', { name: 'Close artifact inspector' }).click()
  await page.setViewportSize({ width: 390, height: 844 })
  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  await expect.soft(page).toHaveScreenshot('artifact-inspector-mobile-light.png', {
    animations: 'disabled',
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('refuses advertised oversized derived views before any content request', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  const oversized = decodeWebBlobDescriptor({
    ...imageArtifact,
    available_views: imageArtifact.available_views.map((view) =>
      view.kind === 'preview' || view.kind === 'thumbnail'
        ? { ...view, byte_length: '16777217' }
        : view,
    ),
  })
  await useArtifactScenario(page, oversized)
  const contentRequests: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/content/')) contentRequests.push(request.url())
  })
  await page.goto('/sessions?workspace=true')
  await submitArtifactWithoutMouse(page, oversized)
  const artifact = page.getByRole('article', { name: 'Artifact orbital-map.png' })
  await expect(artifact.getByText('Details only', { exact: true })).toBeVisible()
  await expect(artifact.locator('img')).toHaveCount(0)
  expect(contentRequests).toEqual([])
  if (testInfo.project.name === 'chromium')
    await expect(page).toHaveScreenshot('derived-byte-bound-desktop.png')
})

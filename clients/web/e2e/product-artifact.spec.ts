import { readFileSync } from 'node:fs'
import {
  imageArtifact as namedImageArtifact,
  jpegDescriptor as namedJpegDescriptor,
} from '../src/features/artifacts/artifactScenario'
import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { openAttachmentConversation } from './attachment-api-fixture'
import { expect, type Page, test } from './fontTest'

// Timeline attachment references do not carry filenames; descriptor URLs echo that input.
const withoutFilename = (descriptor: WebBlobDescriptor): WebBlobDescriptor => ({
  ...descriptor,
  display_filename: [],
  available_views: descriptor.available_views.map((view) => ({
    ...view,
    content_url: view.content_url.replace(/&display_filename=[^&]*/u, ''),
  })),
})
const imageArtifact = withoutFilename(namedImageArtifact)
const jpegDescriptor = withoutFilename(namedJpegDescriptor)

const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const thumbnailFixture = readFileSync(new URL('./fixtures/thumbnail.png', import.meta.url))
const jpegOriginalFixture = readFileSync(new URL('./fixtures/original.jpg', import.meta.url))
const incompatibleDescriptorFixture = { invented: true } as const
const incompatibleDescriptorMessage = 'Unexpected daemon response.'
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

const pane = (page: Page) => page.getByRole('dialog', { name: 'Attachment details' })
const attachment = (page: Page) =>
  page.getByRole('list', { name: 'Attachments' }).getByRole('button').first()

const submitArtifactWithoutMouse = async (page: Page, descriptor = imageArtifact) => {
  await openAttachmentConversation(page, descriptor)
  await attachment(page).focus()
  await page.keyboard.press('Enter')
  await expect(pane(page)).toBeVisible()
  await expect(pane(page).getByRole('textbox')).toHaveCount(0)
}

const resolveArtifactWithoutMouse = async (page: Page, descriptor = imageArtifact) => {
  await submitArtifactWithoutMouse(page, descriptor)
  const displayName = 'Image'
  const artifact = pane(page).getByRole('article', { name: `Artifact ${displayName}` })
  await expect(artifact).toBeVisible()
  await expect(pane(page).getByText(`Found ${displayName}`, { exact: true })).toHaveAttribute(
    'role',
    'status',
  )
  const preview = artifact.getByRole('img', { name: `Preview of ${displayName}` })
  await expect(preview).toBeVisible()
  await expect
    .poll(() => preview.evaluate((element) => (element as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
}

for (const viewport of [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
]) {
  test(`opens conversation attachment details by keyboard at ${viewport.width}px`, async ({
    page,
  }, testInfo) => {
    const problems = watchBrowser(page)
    await useArtifactScenario(page)
    await page.setViewportSize(viewport)
    await resolveArtifactWithoutMouse(page)
    await page.screenshot({ path: testInfo.outputPath('attachment-details.png'), fullPage: true })
    await page.keyboard.press('Escape')
    await expect(pane(page)).toBeHidden()
    await expect(attachment(page)).toBeFocused()
    await expect(page.getByRole('heading', { name: /00000000-0000/ })).toBeVisible()
    expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
  })
}

test('preserves the loaded original when attachment details resize', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page, admittedOriginalArtifact)
  await page.setViewportSize({ width: 1440, height: 900 })
  await resolveArtifactWithoutMouse(page, admittedOriginalArtifact)
  await pane(page).getByRole('button', { name: 'Load original' }).click()
  await expect(pane(page).getByRole('button', { name: 'Original loaded' })).toBeVisible()
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(
    pane(page).getByRole('img', {
      name: `Original of Image`,
    }),
  ).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps background navigation contained while attachment details have focus', async ({
  page,
}) => {
  await useArtifactScenario(page)
  await resolveArtifactWithoutMouse(page)
  const url = page.url()
  await page.keyboard.press('j')
  await page.keyboard.press('Control+k')
  await expect(pane(page)).toBeVisible()
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  expect(page.url()).toBe(url)
  await page.keyboard.press('Escape')
  expect(page.url()).toBe(url)
})

test('keeps an oversized browser-native original download-only', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page, oversizedOriginalArtifact)
  await resolveArtifactWithoutMouse(page, oversizedOriginalArtifact)
  await expect(pane(page).getByRole('button', { name: 'Load original' })).toHaveCount(0)
  await expect(pane(page).getByRole('link', { name: 'Download' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('recovers attachment details after an incompatible descriptor response', async ({ page }) => {
  const scenario = await useRecoveringArtifactScenario(page)
  await submitArtifactWithoutMouse(page)
  await expect(pane(page).getByRole('alert')).toContainText(incompatibleDescriptorMessage)
  scenario.recover()
  await pane(page).getByRole('button', { name: 'Retry', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect(pane(page).getByRole('article')).toBeVisible()
  await expect(pane(page).getByRole('button', { name: 'Close attachment details' })).toBeFocused()
})

test('keeps focus moved during a pending attachment descriptor retry', async ({ page }) => {
  await useRecoveringArtifactScenario(page)
  await submitArtifactWithoutMouse(page)
  await expect(pane(page).getByRole('alert')).toContainText(incompatibleDescriptorMessage)
  const response = Promise.withResolvers<void>()
  await page.route('**/api/blobs/**/descriptor?*', async (route) => {
    await response.promise
    await route.fulfill({ json: imageArtifact })
  })
  const request = page.waitForRequest('**/api/blobs/**/descriptor?*')
  await pane(page).getByRole('button', { name: 'Retry', exact: true }).click()
  await request
  const close = pane(page).getByRole('button', { name: 'Close attachment details' })
  await close.focus()
  response.resolve()
  await expect(pane(page).getByRole('article')).toBeVisible()
  await expect(close).toBeFocused()
})

test('refuses advertised oversized derived views before any content request', async ({ page }) => {
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
  await submitArtifactWithoutMouse(page, oversized)
  const artifact = pane(page).getByRole('article', { name: 'Artifact Image' })
  await expect(artifact.getByText('Details only', { exact: true })).toBeVisible()
  await expect(artifact.locator('img')).toHaveCount(0)
  expect(contentRequests).toEqual([])
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

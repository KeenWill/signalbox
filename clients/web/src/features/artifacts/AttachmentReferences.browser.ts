import { readFileSync } from 'node:fs'
import { expect, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../../product.fixture'
import { fallbackDescriptor, imageDescriptor, jpegDescriptor } from './artifactScenario'

const withoutFilename = (descriptor: typeof imageDescriptor) => ({
  ...descriptor,
  display_filename: [],
  available_views: descriptor.available_views.map((view) => ({
    ...view,
    content_url: view.content_url.replace(/&display_filename=[^&]*/u, ''),
  })),
})
const imageAttachment = withoutFilename(imageDescriptor)
const fileAttachment = withoutFilename(fallbackDescriptor)

const preview = readFileSync(new URL('../../../e2e/fixtures/preview.png', import.meta.url))

test('renders inline images and opens attachment details by keyboard', async ({
  page,
}, testInfo) => {
  const errors: string[] = []
  const originals: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  page.on('request', (request) => {
    if (request.url().includes('/download')) originals.push(request.url())
  })
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) => {
    const descriptor = decodeURIComponent(route.request().url()).includes(imageDescriptor.digest)
      ? imageAttachment
      : fileAttachment
    return route.fulfill({ json: descriptor })
  })
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html')
  await expect(page.getByRole('img', { name: 'Preview of Image' })).toBeVisible()
  const file = page.getByRole('button', {
    name: 'File · application/octet-stream · 4,096 bytes',
    exact: true,
  })
  await expect(file).toBeVisible()
  await file.focus()
  await page.keyboard.press('Enter')
  const pane = page.getByRole('dialog', { name: 'Attachment details' })
  await expect(pane).toBeVisible()
  await expect(pane.getByRole('textbox')).toHaveCount(0)
  await expect(pane.getByRole('link', { name: 'Download' })).toHaveAttribute(
    'href',
    fileAttachment.available_views[0]?.content_url ?? '',
  )
  await page.keyboard.press('Escape')
  await expect(pane).not.toBeVisible()
  await expect(file).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('inline-attachments.png'), fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  await file.click()
  await expect(pane).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({
    path: testInfo.outputPath('attachment-details-phone.png'),
    fullPage: true,
  })
  expect(originals).toEqual([])
  expect(errors).toEqual([])
})

test('does not resolve attachments when blob delivery is disabled', async ({ page }) => {
  const requests: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/api/blobs/')) requests.push(request.url())
  })
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...webContractBootstrapFixture,
        capabilities: {
          ...webContractBootstrapFixture.capabilities,
          immutable_blob_content: false,
        },
      },
    }),
  )
  await page.goto('/src/features/artifacts/scenario.html')
  await expect(page.getByRole('button', { name: /Image · image/ })).toBeDisabled()
  await expect(page.getByText('Loading attachment…')).toHaveCount(0)
  expect(requests).toEqual([])
})

test('defers descriptor reads for an attachment below the viewport', async ({ page }) => {
  const requests: string[] = []
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) => {
    requests.push(decodeURIComponent(route.request().url()))
    return route.fulfill({ json: fileAttachment })
  })
  await page.goto('/src/features/artifacts/scenario.html?offscreen')
  await expect(page.getByRole('heading', { name: 'Conversation attachments' })).toBeVisible()
  await expect(page.getByRole('button', { name: /File · application/ })).toBeEnabled()
  expect(requests).toEqual([])
  await page.getByRole('button', { name: /File · application/ }).scrollIntoViewIfNeeded()
  await expect(page.getByRole('link', { name: 'Download' })).toBeVisible()
  expect(requests.every((url) => url.includes(fileAttachment.digest))).toBe(true)
})

test('releases the original image state when its detail pane closes', async ({ page }) => {
  const original = readFileSync(new URL('../../../e2e/fixtures/original.jpg', import.meta.url))
  const thumbnail = readFileSync(new URL('../../../e2e/fixtures/thumbnail.png', import.meta.url))
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({ json: withoutFilename(jpegDescriptor) }),
  )
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: thumbnail, contentType: 'image/png' }),
  )
  await page.route('**/api/blobs/**/content/image-jpeg', (route) =>
    route.fulfill({ body: original, contentType: 'image/jpeg' }),
  )
  await page.goto('/src/features/artifacts/scenario.html?original')
  await page.getByRole('button', { name: /Image · image\/jpeg/ }).click()
  const pane = page.getByRole('dialog', { name: 'Attachment details' })
  await pane.getByRole('button', { name: 'Load original', exact: true }).click()
  await expect(pane.getByRole('button', { name: 'Original loaded' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByLabel('Original image states')).toHaveText('0')
})

test('restores focus to the attachment after a successful retry', async ({ page }) => {
  let unavailable = true
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({
      json: unavailable
        ? { invalid: true }
        : decodeURIComponent(route.request().url()).includes(imageDescriptor.digest)
          ? imageAttachment
          : fileAttachment,
    }),
  )
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html')
  const retry = page.getByRole('button', { name: 'Retry attachment' }).first()
  await retry.focus()
  unavailable = false
  await page.keyboard.press('Enter')
  await expect(page.getByRole('button', { name: /Image · image\/png/ })).toBeFocused()
})

test('restores detail-pane focus after a successful retry', async ({ page }) => {
  let unavailable = true
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({ json: unavailable ? { invalid: true } : fileAttachment }),
  )
  await page.goto('/src/features/artifacts/scenario.html')
  await page.getByRole('button', { name: /File · application/ }).click()
  const pane = page.getByRole('dialog', { name: 'Attachment details' })
  await pane.getByRole('button', { name: 'Retry', exact: true }).focus()
  unavailable = false
  await page.keyboard.press('Enter')
  await expect(pane.getByRole('button', { name: 'Close attachment details' })).toBeFocused()
})

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
  await page.evaluate(() => {
    document.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' || event.key === 'j')
        document.body.dataset.navigationEscape = 'received'
    })
  })
  await file.focus()
  await page.keyboard.press('Enter')
  const pane = page.getByRole('dialog', { name: 'Attachment details' })
  await expect(pane).toBeVisible()
  await expect(pane.getByRole('textbox')).toHaveCount(0)
  await expect(pane.getByRole('link', { name: 'Download' })).toHaveAttribute(
    'href',
    fileAttachment.available_views[0]?.content_url ?? '',
  )
  await page.keyboard.press('j')
  expect(await page.evaluate(() => document.body.dataset.navigationEscape)).toBeUndefined()
  await page.keyboard.press('Escape')
  await expect(pane).not.toBeVisible()
  await expect(file).toBeFocused()
  expect(await page.evaluate(() => document.body.dataset.navigationEscape)).toBeUndefined()
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

test('keeps attachment space while revisiting an unloaded preview', async ({ page }) => {
  let imageReads = 0
  const reload = Promise.withResolvers<void>()
  await page.setViewportSize({ width: 1000, height: 200 })
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', async (route) => {
    if (!decodeURIComponent(route.request().url()).includes(imageDescriptor.digest))
      return route.fulfill({ json: fileAttachment })
    imageReads += 1
    if (imageReads > 1) await reload.promise
    return route.fulfill({ json: imageAttachment })
  })
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html?revisit')
  await expect(page.getByRole('img', { name: 'Preview of Image' })).toBeVisible()
  const attachment = page.getByRole('listitem').first()
  const initial = await attachment.boundingBox()
  if (!initial) throw new Error('The image attachment must be mounted')
  await page.evaluate((top) => scrollTo(0, top), initial.y + initial.height + 1)
  await expect(page.getByRole('img', { name: 'Preview of Image' })).toHaveCount(0)
  // Revisit the bottom of the reserved preview while the attachment chip is still above view.
  await page.evaluate((top) => scrollTo(0, top), initial.y + initial.height - 80)
  await expect.poll(() => imageReads).toBe(2)
  expect((await attachment.boundingBox())?.height).toBeCloseTo(initial.height)
  reload.resolve()
  await expect(page.getByRole('img', { name: 'Preview of Image' })).toBeVisible()
  expect(imageReads).toBe(2)
})

test('releases the phone height reservation after reloading at desktop width', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({
      json: decodeURIComponent(route.request().url()).includes(imageDescriptor.digest)
        ? imageAttachment
        : fileAttachment,
    }),
  )
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html?revisit')
  const image = page.getByRole('img', { name: 'Preview of Image' })
  await expect(image).toBeVisible()
  const attachment = page.locator('.inline-attachment').first()
  const phone = await attachment.boundingBox()
  if (!phone) throw new Error('Mounted attachment required')
  await page.evaluate((top) => scrollTo(0, top), phone.y + phone.height + 1)
  await expect(image).toHaveCount(0)
  await page.setViewportSize({ width: 1440, height: 844 })
  await page.evaluate(() => scrollTo(0, 0))
  await expect(image).toBeVisible()
  await expect
    .poll(() => attachment.evaluate((element) => (element as HTMLElement).style.minHeight))
    .toBe('')
  const desktop = await attachment.boundingBox()
  if (!desktop) throw new Error('Reloaded attachment required')
  expect(desktop.height).toBeLessThan(phone.height)
})

for (const label of ['garbage', 'image/png;']) {
  test(`downloads attachments carrying timeline label ${label}`, async ({ page }) => {
    const mediaTypes: string[] = []
    await page.route('**/api/bootstrap', (route) =>
      route.fulfill({ json: webContractBootstrapFixture }),
    )
    await page.route('**/api/blobs/**/descriptor?*', (route) => {
      mediaTypes.push(new URL(route.request().url()).searchParams.get('media_type') ?? '')
      return route.fulfill({ json: fileAttachment })
    })
    await page.goto(`/src/features/artifacts/scenario.html?label=${encodeURIComponent(label)}`)
    const chip = page.getByRole('button', {
      name: `File · ${label} · 4,096 bytes`,
      exact: true,
    })
    await expect(page.getByRole('link', { name: 'Download' })).toBeVisible()
    await chip.click()
    await expect(
      page
        .getByRole('dialog', { name: 'Attachment details' })
        .getByRole('link', { name: 'Download' }),
    ).toHaveAttribute('href', fileAttachment.available_views[0]?.content_url ?? '')
    expect(mediaTypes.length).toBeGreaterThan(0)
    expect(mediaTypes.every((type) => type === 'application/octet-stream')).toBe(true)
  })
}

test('restores focus to the image preview control after closing details', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({
      json: decodeURIComponent(route.request().url()).includes(imageDescriptor.digest)
        ? imageAttachment
        : fileAttachment,
    }),
  )
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html')
  const control = page
    .getByRole('article', { name: 'Artifact Image', exact: true })
    .getByRole('button', { name: 'Image Image', exact: true })
  await control.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('dialog', { name: 'Attachment details' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(control).toBeFocused()
})

test('labels uppercase image MIME types as images in previews and details', async ({ page }) => {
  const descriptor = {
    ...imageAttachment,
    declared_media_type: 'IMAGE/PNG',
    available_views: imageAttachment.available_views.map((view) => {
      const url = new URL(view.content_url, 'http://localhost')
      if (url.searchParams.has('media_type')) url.searchParams.set('media_type', 'IMAGE/PNG')
      return {
        ...view,
        media_type: view.kind === 'download' ? 'IMAGE/PNG' : view.media_type,
        content_url: `${url.pathname}${url.search}`,
      }
    }),
  }
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/blobs/**/descriptor?*', (route) => route.fulfill({ json: descriptor }))
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: preview, contentType: 'image/png' }),
  )
  await page.goto('/src/features/artifacts/scenario.html?uppercase')
  await expect(page.getByRole('img', { name: 'Preview of Image', exact: true })).toBeVisible()
  await page.getByRole('button', { name: /Image · IMAGE\/PNG/ }).click()
  const pane = page.getByRole('dialog', { name: 'Attachment details' })
  await expect(pane.getByRole('article', { name: 'Artifact Image', exact: true })).toBeVisible()
  await expect(pane.getByRole('img', { name: 'Preview of Image', exact: true })).toBeVisible()
})

for (const move of ['pointer', 'keyboard']) {
  test(`preserves ${move} focus moves during an inline retry`, async ({ page }) => {
    let retrying = false
    const pending = Promise.withResolvers<void>()
    const requested = Promise.withResolvers<void>()
    await page.route('**/api/bootstrap', (route) =>
      route.fulfill({ json: webContractBootstrapFixture }),
    )
    await page.route('**/api/blobs/**/descriptor?*', async (route) => {
      if (!decodeURIComponent(route.request().url()).includes(imageDescriptor.digest))
        return route.fulfill({ json: fileAttachment })
      if (!retrying) return route.fulfill({ json: { invalid: true } })
      requested.resolve()
      await pending.promise
      return route.fulfill({ json: imageAttachment })
    })
    await page.route('**/api/blobs/**/content/image-png', (route) =>
      route.fulfill({ body: preview, contentType: 'image/png' }),
    )
    await page.goto('/src/features/artifacts/scenario.html')
    const retry = page.getByRole('button', { name: 'Retry attachment' })
    await retry.focus()
    retrying = true
    await page.keyboard.press('Enter')
    await requested.promise
    if (move === 'pointer')
      await page.getByText('Here is the image and the trace file.', { exact: true }).click()
    else await page.keyboard.press('Tab')
    const focused = await page.evaluateHandle(() => document.activeElement)
    pending.resolve()
    await expect(page.getByRole('img', { name: 'Preview of Image' })).toBeVisible()
    await page.evaluate(
      () =>
        new Promise<void>((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
        ),
    )
    expect(await page.evaluate((element) => document.activeElement === element, focused)).toBe(true)
  })
}

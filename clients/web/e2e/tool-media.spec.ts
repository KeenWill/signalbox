import { readFileSync } from 'node:fs'
import { imageDescriptor } from '../src/features/artifacts/artifactScenario'
import { decodeWebBlobDescriptor } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { openAttachmentConversation } from './attachment-api-fixture'
import { expect, test } from './fontTest'

const image = decodeWebBlobDescriptor({
  ...imageDescriptor,
  display_filename: [],
  available_views: imageDescriptor.available_views.map((view) => ({
    ...view,
    content_url: view.content_url.replace(/&display_filename=[^&]*/u, ''),
  })),
})
const preview = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const thumbnail = readFileSync(new URL('./fixtures/thumbnail.png', import.meta.url))

for (const view of ['preview', 'thumbnail'] as const) {
  test(`renders tool raster results through their ${view} route`, async ({ page }, testInfo) => {
    const descriptor = decodeWebBlobDescriptor({
      ...image,
      available_views: image.available_views.filter(
        (entry) => entry.kind === 'download' || entry.kind === view,
      ),
    })
    const derived = descriptor.available_views.find((entry) => entry.kind === view)
    if (!derived) throw new Error('Derived fixture required')
    const requested: string[] = []
    const errors: string[] = []
    page.on('pageerror', (error) => errors.push(error.message))
    await page.route('**/api/bootstrap', (route) =>
      route.fulfill({ json: webContractBootstrapFixture }),
    )
    await page.route('**/api/blobs/**/descriptor?*', (route) => route.fulfill({ json: descriptor }))
    await page.route('**/api/blobs/**/content/**', (route) => {
      requested.push(new URL(route.request().url()).pathname)
      return route.fulfill({
        body: view === 'preview' ? preview : thumbnail,
        contentType: 'image/png',
      })
    })
    await openAttachmentConversation(page, descriptor, {
      digest: descriptor.digest,
      length_bytes: descriptor.byte_length,
      media_type: descriptor.declared_media_type,
      presentation_kind: 'image',
    })
    const media = page.getByRole('list', { name: 'Attachments' })
    const label = view === 'preview' ? 'Preview of Image' : 'Thumbnail of Image'
    await expect(media.getByRole('img', { name: label })).toBeVisible()
    await expect(media).not.toContainText('sha256:')
    await page.getByRole('radio', { name: 'Tools', exact: true }).check()
    await page.getByRole('button', { name: 'Raw', exact: true }).first().click()
    await expect(media.getByRole('img', { name: label })).toBeVisible()
    await media.getByRole('button', { name: 'Image Image', exact: true }).click()
    const pane = page.getByRole('dialog', { name: 'Attachment details' })
    await expect(pane.getByRole('img', { name: label })).toBeVisible()
    await expect(pane).not.toContainText('sha256:')
    await page.keyboard.press('Escape')
    await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
    const row = page.getByRole('row').filter({ hasText: 'Tool batch updated' })
    await row.click()
    await row.getByRole('list', { name: 'Attachments' }).scrollIntoViewIfNeeded()
    await expect(row.getByRole('img', { name: label })).toBeVisible()
    await row.getByRole('button', { name: 'Image Image', exact: true }).click()
    await expect(pane).toBeVisible()
    await page.keyboard.press('Escape')
    expect(requested.length).toBeGreaterThan(0)
    expect(requested.every((path) => path === derived.content_url)).toBe(true)
    await page.setViewportSize({ width: 390, height: 844 })
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    await page.screenshot({ path: testInfo.outputPath(`tool-${view}-phone.png`), fullPage: true })
    expect(errors).toEqual([])
  })
}

for (const [mediaType, label, source] of [
  ['application/pdf', 'PDF', 'tool'],
  ['audio/ogg', 'Audio', 'user'],
  ['video/mp4', 'Video', 'user'],
] as const) {
  test(`opens labeled ${label} ${source} media with a download`, async ({ page }) => {
    const digest = `sha256:${'6f'.repeat(32)}`
    const descriptor = decodeWebBlobDescriptor({
      digest,
      byte_length: '4096',
      declared_media_type: mediaType,
      display_filename: [],
      available_views: [
        {
          kind: 'download',
          media_type: mediaType,
          byte_length: '4096',
          content_url: `/api/blobs/${digest}/download?media_type=${encodeURIComponent(mediaType)}`,
          derivations: [],
        },
      ],
    })
    await page.route('**/api/bootstrap', (route) =>
      route.fulfill({ json: webContractBootstrapFixture }),
    )
    await page.route('**/api/blobs/**/descriptor?*', (route) => route.fulfill({ json: descriptor }))
    await openAttachmentConversation(
      page,
      descriptor,
      source === 'tool'
        ? { digest, length_bytes: '4096', media_type: mediaType, presentation_kind: 'document' }
        : undefined,
    )
    const media = page.getByRole('list', { name: 'Attachments' })
    await expect(media).not.toContainText('sha256:')
    await expect(media.getByRole('link', { name: 'Download' })).toHaveAttribute(
      'href',
      descriptor.available_views[0]?.content_url ?? '',
    )
    await media
      .getByRole('button', { name: `${label} · ${mediaType} · 4,096 bytes`, exact: true })
      .click()
    const pane = page.getByRole('dialog', { name: 'Attachment details' })
    await expect(
      pane.getByRole('article', { name: `Artifact ${label}`, exact: true }),
    ).toBeVisible()
    await expect(pane.getByRole('link', { name: 'Download' })).toBeVisible()
    await expect(pane).not.toContainText('sha256:')
  })
}

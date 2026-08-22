import { readFileSync } from 'node:fs'

import { expect, type Page, type TestInfo, test } from '@playwright/test'
import { imageArtifact } from '../src/features/artifacts/artifactScenario'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '2' },
  capabilities: {
    blob_derivations: true,
    bounded_json: true,
    bounded_session_timeline: true,
    image_derivatives: true,
    immutable_blob_content: true,
    import_discovery: true,
    imported_continuations: true,
    ndjson_streaming: true,
    same_origin_json_mutations: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_timeline_window_bytes: 65_536,
    max_timeline_window_items: 256,
  },
} as const

const previewFixture = readFileSync(new URL('./fixtures/preview.png', import.meta.url))
const incompatibleDescriptorFixture = { invented: true } as const
const incompatibleDescriptorMessage =
  'The descriptor response did not match the generated web contract.'
// Tunable effective ceiling: the 390px inspector's identical content wraps differently between
// the local and CI Linux fallback fonts by 4.0%; keep that allowance local to this evidence.
const MOBILE_ARTIFACT_TEXT_REFLOW_TOLERANCE = 0.045

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

const useArtifactScenario = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/blobs/**/descriptor?*', (route) =>
    route.fulfill({ json: imageArtifact }),
  )
  await page.route('**/api/blobs/**/content/image-png', (route) =>
    route.fulfill({ body: previewFixture, contentType: 'image/png' }),
  )
}

const useRecoveringArtifactScenario = async (page: Page) => {
  const state = { unavailable: true }
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/blobs/**/descriptor?*', (route) => {
    if (state.unavailable) {
      return route.fulfill({ json: incompatibleDescriptorFixture })
    }
    return route.fulfill({ json: imageArtifact })
  })
  return { recover: () => (state.unavailable = false) }
}

const submitArtifactWithoutMouse = async (page: Page) => {
  const openInspector = page.getByRole('button', { name: 'Open artifact inspector' })
  await openInspector.focus()
  await page.keyboard.press('Enter')
  const digest = page.getByRole('textbox', { name: 'Digest' })
  await expect(digest).toBeFocused()
  await page.keyboard.type(imageArtifact.digest)
  await page.keyboard.press('Tab')
  await expect(page.getByRole('combobox', { name: 'Typed presentation' })).toBeFocused()
  await page.keyboard.press('ArrowDown')
  await page.keyboard.press('Tab')
  await page.keyboard.type(imageArtifact.declared_media_type)
  await page.keyboard.press('Tab')
  await page.keyboard.type(imageArtifact.display_filename[0] ?? '')
  await page.keyboard.press('Tab')
  const descriptorRequest = page.waitForRequest('**/api/blobs/**/descriptor?*')
  await page.keyboard.press('Enter')
  const requestUrl = new URL((await descriptorRequest).url())
  expect(requestUrl.pathname).toBe(
    `/api/blobs/${encodeURIComponent(imageArtifact.digest)}/descriptor`,
  )
  expect(requestUrl.searchParams.get('media_type')).toBe(imageArtifact.declared_media_type)
  expect(requestUrl.searchParams.get('display_filename')).toBe(imageArtifact.display_filename[0])
}

const resolveArtifactWithoutMouse = async (page: Page) => {
  await submitArtifactWithoutMouse(page)
  const artifact = page.getByRole('article', {
    name: `Artifact ${imageArtifact.display_filename[0]}`,
  })
  await expect(artifact).toBeVisible()
  const preview = artifact.getByRole('img', {
    name: `Preview of ${imageArtifact.display_filename[0]}`,
  })
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
  await page.goto('/sessions')

  await resolveArtifactWithoutMouse(page)
  await page.keyboard.press('Escape')
  await expect(page.getByRole('heading', { name: 'Selection details' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open artifact inspector' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses a focus-managed artifact sheet on a phone viewport', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/sessions')

  await resolveArtifactWithoutMouse(page)
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Artifact inspector' })).toBeHidden()
  await expect(page.getByRole('button', { name: 'Open artifact inspector' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('discovers the artifact inspector through the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.goto('/sessions')

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
  await page.goto('/sessions')

  await submitArtifactWithoutMouse(page)
  await expect(page.getByRole('alert')).toContainText(incompatibleDescriptorMessage)
  scenario.recover()
  await page.getByRole('button', { name: 'Retry' }).focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('article', { name: `Artifact ${imageArtifact.display_filename[0]}` }),
  ).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop and responsive artifact evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useArtifactScenario(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/sessions')
  await resolveArtifactWithoutMouse(page)

  await expect(page).toHaveScreenshot('artifact-inspector-desktop-dark.png', {
    animations: 'disabled',
  })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot('artifact-inspector-desktop-light.png', {
    animations: 'disabled',
  })
  await page.getByRole('button', { name: 'Close artifact inspector' }).click()
  await page.setViewportSize({ width: 390, height: 844 })
  await resolveArtifactWithoutMouse(page)
  await expect(page).toHaveScreenshot('artifact-inspector-mobile-light.png', {
    animations: 'disabled',
    maxDiffPixelRatio: MOBILE_ARTIFACT_TEXT_REFLOW_TOLERANCE,
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

import { expect, type Page, type TestInfo, test } from '@playwright/test'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { useDeterministicImportApi } from './import-api-fixture'

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('keeps the imported typed artifact view synchronized with keyboard selection', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto('/imports')

  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await expect(page.getByRole('article', { name: 'Artifact Imported entry 1' })).toContainText(
    'Synthetic imported source evidence at immutable position 1.',
  )
  await entries.focus()
  await entries.press('End')
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    '51',
  )
  const blockedArtifact = page.getByRole('article', { name: 'Artifact Imported entry 51' })
  await expect(blockedArtifact).toContainText('Artifact blocked')
  await expect(blockedArtifact).toContainText(
    'No typed renderer is available for this imported content kind.',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('renders unavailable review evidence as a typed committed state', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/reviews')

  const reviewArtifact = page.getByRole('article', { name: 'Artifact Review evidence' })
  await expect(reviewArtifact).toContainText('review evidence artifact')
  await expect(reviewArtifact).toContainText('Artifact blocked')
  await expect(reviewArtifact).toContainText(
    'Review evidence is not exposed by the current daemon contract.',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens the typed inspector from Reviews and restores toolbar focus with Escape', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/reviews')

  const openInspector = page.getByRole('button', { name: 'Open artifact inspector' })
  await openInspector.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('textbox', { name: 'Digest' })).toBeFocused()
  // Escape is deliberately not hijacked while a text field owns focus, so leave the digest field
  // before asserting that the shell returns focus to the toolbar control that opened the inspector.
  await page.getByRole('button', { name: 'Close artifact inspector' }).focus()
  await page.keyboard.press('Escape')
  await expect(openInspector).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the selected imported artifact on a phone viewport', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/imports')

  const importedArtifact = page.getByRole('article', { name: 'Artifact Imported entry 1' })
  await expect(importedArtifact).toBeVisible()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await importedArtifact.scrollIntoViewIfNeeded()
  await expect(page).toHaveScreenshot('imports-artifact-mobile-light.png', {
    animations: 'disabled',
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

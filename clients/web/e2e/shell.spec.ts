import { expect, test, type Page } from '@playwright/test'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

const largeTimelineFixture = {
  path: '/scenario/large-timeline',
  logicalItems: 100_000,
  loadedItems: 360,
  mountedRowsCeiling: 50,
} as const

const largeFleetFixture = {
  path: '/scenario/large-table',
  logicalRows: 1_000_000,
  loadedRows: 480,
  mountedRowsCeiling: 50,
} as const

const watchBrowser = (page: Page): BrowserProblems => {
  const problems: BrowserProblems = { consoleErrors: [], pageErrors: [] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

test.afterEach(async ({ page }, testInfo) => {
  const diagnostics = await page
    .evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
    .catch(() => undefined)
  await testInfo.attach('signalbox-diagnostics', {
    body: JSON.stringify(diagnostics, null, 2),
    contentType: 'application/json',
  })
})

const platformModifier = (page: Page) =>
  page.evaluate(() => /Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control')

test('keeps a six-figure timeline bounded', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeTimelineFixture.path)

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()
  const timelineMounted = Number(await timeline.getAttribute('data-mounted-rows'))
  expect(timelineMounted).toBeLessThan(largeTimelineFixture.mountedRowsCeiling)
  expect(await timeline.getAttribute('data-total-loaded')).toBe(String(largeTimelineFixture.loadedItems))
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalTimeline).toBe(largeTimelineFixture.logicalItems)
  expect(diagnostics?.loadedTimeline).toBe(largeTimelineFixture.loadedItems)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps a million-row fleet table bounded', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeFleetFixture.path)
  const rows = page.getByRole('rowgroup')
  await expect(rows).toBeVisible()
  expect(Number(await rows.getAttribute('data-mounted-rows'))).toBeLessThan(largeFleetFixture.mountedRowsCeiling)
  expect(await rows.getAttribute('data-total-loaded')).toBe(String(largeFleetFixture.loadedRows))
  expect(await rows.getAttribute('data-logical-total')).toBe(String(largeFleetFixture.logicalRows))

  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalFleet).toBe(largeFleetFixture.logicalRows)
  expect(diagnostics?.loadedFleet).toBe(largeFleetFixture.loadedRows)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('leaves modal navigation inactive while text input owns editing', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const filter = page.getByRole('textbox', { name: 'Filter scenarios' })
  await filter.fill('stream')
  await filter.press('j')
  await expect(filter).toHaveValue('streamj')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute('id', 'event-0')

  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Escape returns focus from text editing to the owning timeline', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const filter = page.getByRole('textbox', { name: 'Filter scenarios' })
  await filter.focus()
  await filter.press('Escape')
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('modal timeline navigation selects stable loaded items', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.getByRole('listbox', { name: 'Session timeline' }).focus()
  await page.keyboard.press('j')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute('id', 'event-1')
  await page.keyboard.press('g')
  await page.keyboard.press('g')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute('id', 'event-0')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Mod+K and Escape open and close the registered command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned dark workbench', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'Chromium owns pixel evidence')
  const problems = watchBrowser(page)
  await page.goto('/scenario/approval')
  await expect(page.getByRole('heading', { name: 'Bounded timeline' })).toBeVisible()
  await expect(page).toHaveScreenshot('workbench-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned light focus layout', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'Chromium owns pixel evidence')
  const problems = watchBrowser(page)
  await page.goto('/scenario/huge-source')
  await page.getByRole('button', { name: 'Switch to focus layout' }).click()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page.getByRole('heading', { name: 'Fleet obligations' })).toBeHidden()
  await expect(page).toHaveScreenshot('focus-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned narrow responsive shell', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'Chromium owns pixel evidence')
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/responsive')
  await expect(page.getByRole('button', { name: 'Open scenarios' })).toBeVisible()
  await expect(page).toHaveScreenshot('responsive-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

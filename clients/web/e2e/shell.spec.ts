import { expect, type Page, type TestInfo, test } from '@playwright/test'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

// Tunable effective ceiling: fewer than 50 mounted rows leaves ample overscan headroom while
// still failing if either virtualized surface materializes its complete bounded window.
const VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING = 50

const largeTimelineFixture = {
  path: '/scenario/large-timeline',
  logicalItems: 100_000,
  loadedItems: 360,
  mountedRowsCeiling: VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
  firstItemTestId: 'timeline-event-0',
  firstItemPosition: '1',
  setSize: '360',
} as const

const largeFleetFixture = {
  path: '/scenario/large-table',
  logicalRows: 1_000_000,
  loadedRows: 480,
  mountedRowsCeiling: VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
  ariaRowCount: '1000001',
  firstRowTestId: 'fleet-obligation-0',
  firstRowIndex: '2',
} as const

const sessionFoundationFixture = {
  path: '/scenario/session-foundation',
  logicalItems: 1_000_000,
  loadedItems: 256,
  mountedRowsCeiling: VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
  latestItemTestId: 'timeline-event-1000000',
} as const

const streamingFixture = {
  firstLoadedItemId: 'event-0',
  secondLoadedItemId: 'event-1',
  lastLoadedItemId: 'event-239',
} as const

const cachedScenarioFixture = {
  streamingFleetSummary: '180 logical · 180 loaded',
  approvalFleetSummary: '240 logical · 240 loaded',
  firstFleetRowTestId: 'fleet-obligation-0',
  initialQueryCacheSummary: '2 bounded entries',
  retainedQueryCacheSummary: '4 bounded entries',
} as const

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

test.afterEach(async ({ page }, testInfo) => {
  const diagnostics = await page
    .evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
    .catch(() => undefined)
  await testInfo.attach('signalbox-diagnostics', {
    body: JSON.stringify(diagnostics ?? null, null, 2),
    contentType: 'application/json',
  })
})

const platformModifier = (page: Page) =>
  page.evaluate(() => (/Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control'))

test('keeps a six-figure timeline bounded', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeTimelineFixture.path)

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()
  const timelineMounted = Number(await timeline.getAttribute('data-mounted-rows'))
  expect(timelineMounted).toBeLessThan(largeTimelineFixture.mountedRowsCeiling)
  expect(await timeline.getAttribute('data-total-loaded')).toBe(
    String(largeTimelineFixture.loadedItems),
  )
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalTimeline).toBe(largeTimelineFixture.logicalItems)
  expect(diagnostics?.loadedTimeline).toBe(largeTimelineFixture.loadedItems)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('browses an enormous production-shaped session from its bounded tail', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(sessionFoundationFixture.path)

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()
  await expect(timeline).toHaveAttribute('data-mounted-rows', /^\d+$/)
  expect(Number(await timeline.getAttribute('data-mounted-rows'))).toBeLessThan(
    sessionFoundationFixture.mountedRowsCeiling,
  )
  expect(await timeline.getAttribute('data-total-loaded')).toBe(
    String(sessionFoundationFixture.loadedItems),
  )
  await timeline.press('End')
  await expect(page.getByTestId(sessionFoundationFixture.latestItemTestId)).toHaveAttribute(
    'aria-selected',
    'true',
  )
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalTimeline).toBe(sessionFoundationFixture.logicalItems)
  expect(diagnostics?.loadedTimeline).toBe(sessionFoundationFixture.loadedItems)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('exposes logical positions for virtualized timeline options', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeTimelineFixture.path)

  const firstItem = page.getByTestId(largeTimelineFixture.firstItemTestId)
  await expect(firstItem).toHaveAttribute('aria-posinset', largeTimelineFixture.firstItemPosition)
  await expect(firstItem).toHaveAttribute('aria-setsize', largeTimelineFixture.setSize)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps a million-row fleet table bounded', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeFleetFixture.path)
  const rows = page.getByRole('rowgroup')
  await expect(rows).toBeVisible()
  expect(Number(await rows.getAttribute('data-mounted-rows'))).toBeLessThan(
    largeFleetFixture.mountedRowsCeiling,
  )
  expect(await rows.getAttribute('data-total-loaded')).toBe(String(largeFleetFixture.loadedRows))
  expect(await rows.getAttribute('data-logical-total')).toBe(String(largeFleetFixture.logicalRows))

  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalFleet).toBe(largeFleetFixture.logicalRows)
  expect(diagnostics?.loadedFleet).toBe(largeFleetFixture.loadedRows)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('exposes logical positions for virtualized fleet rows', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeFleetFixture.path)

  await expect(page.getByRole('table', { name: 'Fleet obligations' })).toHaveAttribute(
    'aria-rowcount',
    largeFleetFixture.ariaRowCount,
  )
  await expect(page.getByTestId(largeFleetFixture.firstRowTestId)).toHaveAttribute(
    'aria-rowindex',
    largeFleetFixture.firstRowIndex,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('makes the fleet scroll viewport keyboard reachable', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(largeFleetFixture.path)

  const rows = page.getByRole('rowgroup', { name: 'Fleet rows' })
  await rows.focus()
  await expect(rows).toBeFocused()
  await rows.press('End')
  expect(await rows.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('leaves modal navigation inactive while text input owns editing', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const filter = page.getByRole('textbox', { name: 'Filter scenarios' })
  await filter.fill('stream')
  await filter.press('j')
  await expect(filter).toHaveValue('streamj')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.firstLoadedItemId,
  )

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

test('modal j selects the next loaded timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.getByRole('listbox', { name: 'Session timeline' }).focus()
  await page.keyboard.press('j')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.secondLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('modal g g selects the first loaded timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.locator(`#${streamingFixture.secondLoadedItemId}`).click()
  await page.getByRole('listbox', { name: 'Session timeline' }).focus()
  await page.keyboard.press('g')
  await page.keyboard.press('g')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.firstLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('ArrowDown selects the next announced timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.focus()
  await timeline.press('ArrowDown')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.secondLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('ArrowUp selects the previous announced timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.locator(`#${streamingFixture.secondLoadedItemId}`).click()
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.focus()
  await timeline.press('ArrowUp')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.firstLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('End selects the final loaded timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.focus()
  await timeline.press('End')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.lastLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Home selects the first loaded timeline item', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.locator(`#${streamingFixture.secondLoadedItemId}`).click()
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.focus()
  await timeline.press('Home')
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.firstLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps the active timeline option mounted after wheel-range scrolling', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toHaveAttribute(
    'aria-activedescendant',
    streamingFixture.firstLoadedItemId,
  )
  await timeline.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(page.locator(`#${streamingFixture.firstLoadedItemId}`)).toBeAttached()
  await expect(timeline).toHaveAttribute(
    'aria-activedescendant',
    streamingFixture.firstLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps virtual-range telemetry out of recent user actions', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.recentActions).toContain('app/timelineSelected')
  expect(diagnostics?.recentActions).not.toContain('app/transcriptRangeSet')
  expect(diagnostics?.recentActions).not.toContain('app/tableRangeSet')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('results detail preserves an eligible selected record', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await timeline.focus()
  await timeline.press('End')
  await page.getByRole('button', { name: 'results' }).click()
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.lastLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('changing scenarios resets timeline selection', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.locator(`#${streamingFixture.secondLoadedItemId}`).click()
  await page.getByRole('link', { name: /Approval required/ }).click()
  await expect(page).toHaveURL(/\/scenario\/approval$/)
  await expect(page.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    streamingFixture.firstLoadedItemId,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('changing to a cached scenario resets timeline scrolling', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.getByRole('link', { name: /Approval required/ }).click()
  await page.getByRole('link', { name: /Streaming session/ }).click()
  const streamingTimeline = page.getByRole('listbox', { name: 'Session timeline' })
  await streamingTimeline.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  expect(await streamingTimeline.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)

  await page.getByRole('link', { name: /Approval required/ }).click()
  const approvalTimeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(page).toHaveURL(/\/scenario\/approval$/)
  await expect(page.locator(`#${streamingFixture.firstLoadedItemId}`)).toBeInViewport()
  expect(await approvalTimeline.evaluate((element) => element.scrollTop)).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('changing to a cached scenario resets fleet scrolling', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.getByRole('link', { name: /Approval required/ }).click()
  await expect(
    page.getByText(cachedScenarioFixture.approvalFleetSummary, { exact: true }),
  ).toBeVisible()
  await expect(
    page.getByText(cachedScenarioFixture.streamingFleetSummary, { exact: true }),
  ).toBeHidden()
  await page.getByRole('link', { name: /Streaming session/ }).click()
  await expect(
    page.getByText(cachedScenarioFixture.streamingFleetSummary, { exact: true }),
  ).toBeVisible()
  await expect(
    page.getByText(cachedScenarioFixture.approvalFleetSummary, { exact: true }),
  ).toBeHidden()
  const streamingFleet = page.getByRole('rowgroup', { name: 'Fleet rows' })
  await streamingFleet.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  expect(await streamingFleet.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)

  await page.getByRole('link', { name: /Approval required/ }).click()
  await expect(
    page.getByText(cachedScenarioFixture.approvalFleetSummary, { exact: true }),
  ).toBeVisible()
  await expect(
    page.getByText(cachedScenarioFixture.streamingFleetSummary, { exact: true }),
  ).toBeHidden()
  const approvalFleet = page.getByRole('rowgroup', { name: 'Fleet rows' })
  await expect(page).toHaveURL(/\/scenario\/approval$/)
  await expect(page.getByTestId(cachedScenarioFixture.firstFleetRowTestId)).toBeInViewport()
  expect(await approvalFleet.evaluate((element) => element.scrollTop)).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('reports retained scenario queries in the query cache', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const diagnostics = page.getByRole('complementary', { name: 'Diagnostics' })
  await expect(diagnostics.getByText(cachedScenarioFixture.initialQueryCacheSummary)).toBeVisible()
  await page.getByRole('link', { name: /Approval required/ }).click()
  await expect(diagnostics.getByText(cachedScenarioFixture.retainedQueryCacheSummary)).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('marks only the active scenario link as the current page', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await expect(page.getByRole('link', { name: /Streaming session/ })).toHaveAttribute(
    'aria-current',
    'page',
  )
  await expect(page.getByRole('link', { name: /Approval required/ })).not.toHaveAttribute(
    'aria-current',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('scenario selection closes mobile navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/streaming')

  await page.getByRole('button', { name: 'Open scenarios' }).click()
  const navigation = page.getByRole('dialog', { name: 'Development scenarios' })
  await navigation.getByRole('link', { name: /Approval required/ }).click()
  await expect(navigation).toBeHidden()
  await expect(page).toHaveURL(/\/scenario\/approval$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps the fleet surface reachable on a short mobile viewport', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 667, height: 320 })
  await page.goto('/scenario/responsive')

  await expect(page.getByRole('heading', { name: 'Fleet obligations' })).toBeVisible()
  await expect(page.getByTestId(cachedScenarioFixture.firstFleetRowTestId)).toBeInViewport({
    ratio: 1,
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps the fleet surface reachable on a short wide viewport', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 800, height: 320 })
  await page.goto('/scenario/responsive')

  await expect(page.getByRole('heading', { name: 'Fleet obligations' })).toBeVisible()
  await expect(page.getByTestId(cachedScenarioFixture.firstFleetRowTestId)).toBeInViewport({
    ratio: 1,
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Mod+K opens the registered command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Escape closes the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('the command palette opens keyboard help without closing it', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned dark workbench', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto('/scenario/approval')
  await expect(page.getByRole('heading', { name: 'Bounded timeline' })).toBeVisible()
  await expect(page).toHaveScreenshot('workbench-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned light focus layout', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto('/scenario/huge-source')
  await page.getByRole('button', { name: 'Switch to focus layout' }).click()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page.getByRole('heading', { name: 'Fleet obligations' })).toBeHidden()
  await expect(page).toHaveScreenshot('focus-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned narrow responsive shell', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/scenario/responsive')
  await expect(page.getByRole('button', { name: 'Open scenarios' })).toBeVisible()
  await expect(page).toHaveScreenshot('responsive-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

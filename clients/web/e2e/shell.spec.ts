import { expect, type Page, type TestInfo, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'

interface BrowserProblems {
  consoleErrors: string[]
  pageErrors: string[]
}

// Tunable effective ceiling: fewer than 50 mounted rows leaves ample overscan headroom while
// still failing if either virtualized surface materializes its complete bounded window.
const VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING = 50

// Tunable effective ceiling for the densest text surface. The usage screen renders the scenario
// sidebar, a six-column call table, and three aggregate cards at once, so host-to-host font
// metric differences change line wrapping rather than only rasterizing glyphs differently. The
// drift between this golden and the CI runner's rendering of the same commit measures 3.75%,
// above the shared cross-host ceiling in playwright.config.ts, so the bound is widened here
// alone and every other screenshot keeps the tighter global one.
const USAGE_TEXT_DENSITY_TOLERANCE = 0.045

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
  timelineHeading: 'Bounded timeline',
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

const searchUsageFixture = {
  searchPath: '/scenario/search-usage?view=search&q=needle&searchScope=session',
  usagePath: '/scenario/search-usage?view=usage&usageSession=all&usageOrder=newest',
  farTimelineItemId: 'event-777777',
  searchLoadedItems: '72',
  usageLoadedCalls: '100',
  revealedTimelineItems: '12',
  mountedRowsCeiling: VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
} as const

const importsFixture = {
  path: '/scenario/imports',
  logicalImports: 1_000_000,
  loadedImports: 100,
  logicalEntries: 250_000,
  latestWindowSummary: '249,950–250,000 · 51 loaded',
  arbitraryWindowSummary: '124,975–125,025 · 51 loaded',
  continuedSessionId: '00000000-0000-7000-8000-000009000000',
  firstEntryId: 'import-entry-00000000-0000-7000-8000-000002000001',
  secondEntryId: 'import-entry-00000000-0000-7000-8000-000002000002',
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
  await expect.poll(() => rows.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
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
  await expect(page.getByRole('heading', { name: streamingFixture.timelineHeading })).toBeVisible()

  await expect(page.getByRole('button', { name: 'Open command palette' })).toBeVisible()
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

test('the command palette opens keyboard help with available product navigation', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.goto('/scenario/streaming')
  await expect(page.getByRole('heading', { name: streamingFixture.timelineHeading })).toBeVisible()

  await expect(page.getByRole('button', { name: 'Open command palette' })).toBeVisible()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  const help = page.getByRole('dialog', { name: 'Keyboard help' })
  await expect(help).toBeVisible()
  await expect(help.getByText('Go to Attention', { exact: true })).toBeVisible()
  await expect(help.getByText('Go to Sessions', { exact: true })).toBeVisible()
  await expect(help.getByText('Go to Settings', { exact: true })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns from the scenario studio through the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  const attentionSnapshot = {
    continuation_after_session_id: null,
    cursor: '0',
    summaries: [],
  }
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionSnapshot })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => route.fulfill({ json: attentionSnapshot }))
  await page.goto('/scenario/streaming')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Go to Attention/ }).click()

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('reveals a lexical hit far outside the loaded timeline window', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.searchPath)

  const results = page.getByRole('listbox', { name: 'Lexical search results' })
  await expect(results).toHaveAttribute('data-total-loaded', searchUsageFixture.searchLoadedItems)
  await results.focus()
  await results.press('Enter')
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline.getByRole('option', { selected: true })).toHaveAttribute(
    'id',
    searchUsageFixture.farTimelineItemId,
  )
  await expect(timeline).toHaveAttribute(
    'data-total-loaded',
    searchUsageFixture.revealedTimelineItems,
  )
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_SEARCH_USAGE_DIAGNOSTICS__?.())
  expect(diagnostics?.transcriptRevealReads).toBe(1)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps a derived-artifact search projection virtualized', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.searchPath)

  const results = page.getByRole('listbox', { name: 'Lexical search results' })
  await expect(results).toHaveAttribute('data-total-loaded', searchUsageFixture.searchLoadedItems)
  expect(Number(await results.getAttribute('data-mounted-rows'))).toBeLessThan(
    searchUsageFixture.mountedRowsCeiling,
  )
  await expect(results.getByRole('option').first()).toContainText('derived text artifact')
  await expect(
    results.getByRole('option').first().getByText('needle', { exact: true }),
  ).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('renders mixed usage evidence without scanning the transcript', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.usagePath)

  const summaries = page.getByRole('region', { name: 'Usage summary groups' })
  await expect(summaries).toContainText('reported')
  await expect(summaries).toContainText('estimated')
  await expect(summaries).toContainText('rates-2026-08-a')
  await expect(summaries).toContainText('rates-2026-08-b')
  await expect(summaries).toContainText('metered equivalent')
  await expect(summaries).toContainText('out —')
  const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', searchUsageFixture.usageLoadedCalls)
  expect(Number(await rows.getAttribute('data-mounted-rows'))).toBeLessThan(
    searchUsageFixture.mountedRowsCeiling,
  )
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_SEARCH_USAGE_DIAGNOSTICS__?.())
  expect(diagnostics).toEqual({
    searchReads: 0,
    usageSummaryReads: 1,
    usageCallReads: 1,
    transcriptRevealReads: 0,
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps usage drill-down filters in typed URL state', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.usagePath)

  const summaries = page.getByRole('region', { name: 'Usage summary groups' })
  await summaries.getByRole('button').first().click()
  await expect(page).toHaveURL(/modelId=00000000-0000-0000-0000-000000001001/)
  await expect(page).toHaveURL(/provenance=reported/)
  await expect(page).toHaveURL(/callKind=model_call/)
  await expect(page.getByRole('button', { name: 'Clear drill-down' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('focuses lexical search with its registered hotkey', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.searchPath)
  // The scenario workspace is lazy-loaded; wait for the search surface before
  // pressing so the hotkey lands on registered handlers, not the loading shell.
  const searchInput = page.getByRole('textbox', { name: 'Search canonical session evidence' })
  await expect(searchInput).toBeVisible()

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+Shift+F`)
  await expect(searchInput).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures bounded search evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.searchPath)
  await expect(page.getByRole('listbox', { name: 'Lexical search results' })).toHaveAttribute(
    'data-total-loaded',
    searchUsageFixture.searchLoadedItems,
  )
  await expect(page).toHaveScreenshot('search-usage-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures mixed usage and cost evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto(searchUsageFixture.usagePath)
  await expect(page.getByRole('rowgroup', { name: 'Usage call rows' })).toHaveAttribute(
    'data-total-loaded',
    searchUsageFixture.usageLoadedCalls,
  )
  await expect(page).toHaveScreenshot('usage-dark.png', {
    animations: 'disabled',
    maxDiffPixelRatio: USAGE_TEXT_DENSITY_TOLERANCE,
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps million-row imports and enormous entry histories bounded', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const imports = page.getByRole('rowgroup', { name: 'Imported conversation rows' })
  await expect(imports).toHaveAttribute('data-total-loaded', String(importsFixture.loadedImports))
  expect(Number(await imports.getAttribute('data-mounted-rows'))).toBeLessThan(
    VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
  )
  await expect(page.getByText('250,000', { exact: true }).first()).toBeVisible()
  const diagnostics = await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__?.())
  expect(diagnostics?.logicalImports).toBe(importsFixture.logicalImports)
  expect(diagnostics?.loadedImports).toBe(importsFixture.loadedImports)

  await page.getByRole('button', { name: 'Latest', exact: true }).click()
  await expect(page.getByText(importsFixture.latestWindowSummary, { exact: true })).toBeVisible()
  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await expect(entries).toHaveAttribute('data-total-loaded', '51')
  expect(Number(await entries.getAttribute('data-mounted-rows'))).toBeLessThan(
    VIRTUALIZED_MOUNTED_ROWS_EXCLUSIVE_CEILING,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('navigates imported frontiers by keyboard and preserves logical positions', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await entries.focus()
  await entries.press('j')
  await expect(entries).toHaveAttribute('aria-activedescendant', importsFixture.secondEntryId)
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    '2',
  )
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-setsize',
    String(importsFixture.logicalEntries),
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('leaves imported modal navigation inactive while position input owns editing', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const position = page.getByRole('textbox', { name: 'Imported entry position' })
  await position.fill('125000')
  await position.press('j')
  await expect(position).toHaveValue('125000j')
  await expect(page.getByRole('listbox', { name: 'Imported source entries' })).toHaveAttribute(
    'aria-activedescendant',
    'import-entry-00000000-0000-7000-8000-000002000001',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Escape returns imported position editing to its owning entry window', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const position = page.getByRole('textbox', { name: 'Imported entry position' })
  await position.focus()
  await position.press('Escape')
  await expect(page.getByRole('listbox', { name: 'Imported source entries' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('discovers imported navigation bindings through the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const entries = page.locator('[aria-label="Imported source entries"]')
  await expect(entries).toHaveAttribute('aria-activedescendant', importsFixture.firstEntryId)
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette.getByText('Select next imported frontier', { exact: true })).toBeVisible()
  await expect(palette.getByText('j', { exact: true })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends imported navigation while the command palette owns focus', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  const entries = page.locator('[aria-label="Imported source entries"]')
  await expect(entries).toHaveAttribute('aria-activedescendant', importsFixture.firstEntryId)
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('j')

  await expect(entries).toHaveAttribute('aria-activedescendant', importsFixture.firstEntryId)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('continues an exact arbitrary imported frontier as a native session', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)

  await page.getByRole('textbox', { name: 'Imported entry position' }).fill('125000')
  await page.getByRole('button', { name: 'Go', exact: true }).press('Enter')
  await expect(page.getByText(importsFixture.arbitraryWindowSummary, { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Resume', exact: true }).press('Enter')
  await expect(
    page.getByText(`Session created: ${importsFixture.continuedSessionId}`, { exact: true }),
  ).toBeVisible()
  await expect(page.getByText('Imported source', { exact: true }).first()).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the pinned imports workstation', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await page.goto(importsFixture.path)
  await expect(page.getByRole('heading', { name: 'Imported conversations' })).toBeVisible()
  await expect(page).toHaveScreenshot('imports-dark.png', { animations: 'disabled' })
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

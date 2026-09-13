import { expect, test } from '@playwright/test'

test('filters usage and keeps dollars visible on a phone', async ({ page }, testInfo) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/src/search-usage/preview.html')
  const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', '100')
  expect(Number(await rows.getAttribute('data-mounted-rows'))).toBeLessThan(60)
  await expect(page.getByRole('columnheader', { name: 'Cost' })).toBeVisible()
  await expect(rows).toContainText('unpriced')
  await page
    .getByLabel('Model', { exact: true })
    .selectOption('00000000-0000-0000-0000-000000001003')
  await expect(rows).toHaveAttribute('data-total-loaded', '48')
  await expect(rows).toContainText('unpriced · 00000000-0000-0000-0000-000000001003')
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('columnheader', { name: 'Cost' })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('usage-phone.png'), fullPage: true })
  expect(errors).toEqual([])
})

test('loads session and turn chips through the HTTP usage client', async ({ page }, testInfo) => {
  const { webContractBootstrapFixture } = await import('../product.fixture')
  const { SearchUsageScenarioSource, SEARCH_USAGE_SCENARIO_SESSION_ID } = await import('./scenario')
  const source = new SearchUsageScenarioSource()
  const calls = await source.usageCalls({ filters: {}, order: 'newest', maxItems: 100 })
  const turnId = calls.calls[0]?.turn_id
  if (!turnId) throw new Error('Fixture must have a turn')
  const requests: URL[] = []
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    requests.push(url)
    if (url.pathname === '/api/bootstrap')
      return route.fulfill({ json: webContractBootstrapFixture })
    if (url.pathname === '/api/usage/calls') return route.fulfill({ json: calls })
    if (url.pathname === '/api/usage/summary')
      return route.fulfill({ json: await source.usageSummary({}) })
    throw new Error(`Unexpected endpoint ${url.pathname}`)
  })
  await page.goto(
    `/src/search-usage/preview.html?preview=cost&session=${SEARCH_USAGE_SCENARIO_SESSION_ID}&turn=${turnId}`,
  )
  await expect(page.getByRole('region', { name: 'Session cost', exact: true })).toContainText(
    'unpriced',
  )
  await expect(page.getByRole('region', { name: 'Turn cost', exact: true })).toContainText(
    'unpriced',
  )
  await expect(page.getByRole('region', { name: 'Recent turn costs' })).toContainText('partial')
  await expect(page.getByRole('link')).toHaveCount(0)
  const session = page.getByRole('region', { name: 'Session cost', exact: true })
  const toggle = session.getByRole('button')
  const pricing = session.getByText(/Reported · Metered cost · rates-2026-08-a/)
  await expect(pricing).toBeHidden()
  await toggle.focus()
  await page.keyboard.press('Enter')
  await expect(toggle).toHaveAttribute('aria-expanded', 'true')
  await expect(pricing).toBeVisible()
  await expect(pricing).toContainText('Estimated · Equivalent metered cost · rates-2026-08-b')
  await page.keyboard.press('Space')
  await expect(pricing).toBeHidden()
  await page.setViewportSize({ width: 390, height: 844 })
  await toggle.click()
  await expect(pricing).toBeVisible()
  expect(await session.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('cost-details-phone.png'), fullPage: true })
  await toggle.click()
  await page.setViewportSize({ width: 1280, height: 844 })
  await page.screenshot({ path: testInfo.outputPath('cost-chips.png') })
  expect(requests.filter((url) => url.pathname === '/api/bootstrap')).toHaveLength(1)
  expect(
    requests
      .filter((url) => url.pathname === '/api/usage/summary')
      .map((url) => url.searchParams.get('turn_id')),
  ).toEqual([null, turnId])
  expect(
    requests
      .filter((url) => url.pathname.startsWith('/api/usage/'))
      .every((url) => url.searchParams.get('session_id') === SEARCH_USAGE_SCENARIO_SESSION_ID),
  ).toBe(true)
})

test('retries the connection when a session cost refresh follows bootstrap failure', async ({
  page,
}) => {
  const { webContractBootstrapFixture } = await import('../product.fixture')
  const { SEARCH_USAGE_SCENARIO_SESSION_ID } = await import('./scenario')
  let unavailable = true
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === '/api/bootstrap')
      return unavailable
        ? route.fulfill({ status: 503, body: 'Service unavailable' })
        : route.fulfill({ json: webContractBootstrapFixture })
    if (url.pathname === '/api/usage/calls')
      return route.fulfill({ json: { calls: [], continuation: null } })
    return route.fulfill({ json: { groups: [], truncated: false } })
  })
  await page.goto(
    `/src/search-usage/preview.html?preview=cost&session=${SEARCH_USAGE_SCENARIO_SESSION_ID}`,
  )
  const session = page.getByRole('region', { name: 'Session cost', exact: true })
  await expect(session).toHaveText('Cost unavailable')
  unavailable = false
  await page.getByRole('button', { name: 'Refresh costs' }).click()
  await expect(session.getByRole('button')).toHaveText('$0')
})

test('does not show session totals as turn cost when no turn is selected', async ({ page }) => {
  const { webContractBootstrapFixture } = await import('../product.fixture')
  const { SearchUsageScenarioSource, SEARCH_USAGE_SCENARIO_SESSION_ID } = await import('./scenario')
  const source = new SearchUsageScenarioSource()
  const summaryRequests: URL[] = []
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === '/api/bootstrap')
      return route.fulfill({ json: webContractBootstrapFixture })
    if (url.pathname === '/api/usage/calls')
      return route.fulfill({ json: { calls: [], continuation: null } })
    if (url.pathname === '/api/usage/summary') {
      summaryRequests.push(url)
      return route.fulfill({ json: await source.usageSummary({}) })
    }
    throw new Error(`Unexpected endpoint ${url.pathname}`)
  })
  await page.goto(
    `/src/search-usage/preview.html?preview=cost&session=${SEARCH_USAGE_SCENARIO_SESSION_ID}`,
  )
  await expect(page.getByRole('region', { name: 'Session cost', exact: true })).toContainText(
    '$2.84',
  )
  const turn = page.getByRole('region', { name: 'Turn cost', exact: true })
  await expect(turn).toHaveText('Cost not loaded')
  await expect(turn.getByRole('button')).toHaveCount(0)
  expect(summaryRequests).toHaveLength(1)
  expect(summaryRequests[0]?.searchParams.get('session_id')).toBe(SEARCH_USAGE_SCENARIO_SESSION_ID)
  expect(summaryRequests[0]?.searchParams.has('turn_id')).toBe(false)
})

test('keeps filters in the route across reload and history navigation', async ({ page }) => {
  await page.goto('/src/search-usage/preview.html')
  const model = page.getByLabel('Model', { exact: true })
  const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', '100')
  await model.selectOption('00000000-0000-0000-0000-000000001003')
  await expect(page).toHaveURL(/model=00000000-0000-0000-0000-000000001003/)
  await expect(rows).toHaveAttribute('data-total-loaded', '48')
  await page.reload()
  await expect(model).toHaveValue('00000000-0000-0000-0000-000000001003')
  await expect(rows).toHaveAttribute('data-total-loaded', '48')
  await page.getByLabel('From', { exact: true }).fill('2026-08-22T12:30')
  await expect(page).toHaveURL(/from=/)
  await page.goBack()
  await expect(page.getByLabel('From', { exact: true })).toHaveValue('')
  await page.goForward()
  await expect(page.getByLabel('From', { exact: true })).toHaveValue('2026-08-22T12:30')
  await page.goBack()
  await model.selectOption('')
  await expect(rows).toHaveAttribute('data-total-loaded', '100')
  await page.goBack()
  await expect(model).toHaveValue('00000000-0000-0000-0000-000000001003')
  await expect(rows).toHaveAttribute('data-total-loaded', '48')
})

test('mounts loaded subtotals only while inspected at the six-page limit', async ({ page }) => {
  const { webContractBootstrapFixture } = await import('../product.fixture')
  const { SearchUsageScenarioSource } = await import('./scenario')
  const source = new SearchUsageScenarioSource()
  const fixture = (await source.usageCalls({ filters: {}, order: 'newest', maxItems: 1 })).calls[0]
  if (!fixture) throw new Error('Fixture must contain a call')
  let nextPage = 0
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === '/api/bootstrap')
      return route.fulfill({ json: webContractBootstrapFixture })
    if (url.pathname === '/api/usage/summary')
      return route.fulfill({ json: { groups: [], truncated: false } })
    const calls = Array.from({ length: 100 }, (_, offset) => {
      const index = nextPage * 100 + offset
      const id = `00000000-0000-0000-0000-${String(10000 - index).padStart(12, '0')}`
      return {
        ...fixture,
        call_id: id,
        session_id: id,
        turn_id: id,
        recorded_at_micros: String(Number(fixture.recorded_at_micros) - index),
      }
    })
    nextPage += 1
    const last = calls.at(-1)
    await route.fulfill({
      json: {
        calls,
        continuation:
          nextPage < 6 && last
            ? { call_id: last.call_id, recorded_at_micros: last.recorded_at_micros }
            : null,
      },
    })
  })
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/src/search-usage/preview.html?http')
  const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
  for (let count = 100; count <= 600; count += 100) {
    await expect(rows).toHaveAttribute('data-total-loaded', String(count))
    if (count < 600) await page.getByRole('button', { name: 'Load more' }).click()
  }
  const subtotals = page.getByRole('region', { name: 'Loaded cost subtotals', includeHidden: true })
  await expect(subtotals).toHaveCount(0)
  expect(Number(await rows.getAttribute('data-mounted-rows'))).toBeLessThan(60)
  const disclosure = page.getByText('Session and turn costs in loaded calls', { exact: true })
  await disclosure.click()
  await expect(subtotals.getByRole('button', { name: 'Show session usage' })).toHaveCount(600)
  await expect(subtotals).toContainText('incomplete')
  await disclosure.click()
  await expect(subtotals).toHaveCount(0)
})

test('reports missing session context without a loading status or request', async ({ page }) => {
  const requests: string[] = []
  await page.route('**/api/**', async (route) => {
    requests.push(route.request().url())
    await route.fulfill({ status: 500, body: 'No request should run without a session' })
  })
  await page.goto(
    '/src/search-usage/preview.html?preview=cost&turn=00000000-0000-0000-0000-000000000001',
  )
  await expect(page.getByRole('region', { name: 'Session cost', exact: true })).toHaveText(
    'Cost not loaded',
  )
  await expect(page.getByRole('region', { name: 'Turn cost', exact: true })).toHaveText(
    'Cost not loaded',
  )
  expect(requests).toEqual([])
})

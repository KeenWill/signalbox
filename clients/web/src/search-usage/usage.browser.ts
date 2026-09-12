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
  await expect(
    page.getByRole('region', { name: 'Session cost', exact: true }).getByRole('link'),
  ).toContainText('unpriced')
  await expect(
    page.getByRole('region', { name: 'Turn cost', exact: true }).getByRole('link'),
  ).toHaveAttribute('href', `/usage?session=${SEARCH_USAGE_SCENARIO_SESSION_ID}&turn=${turnId}`)
  await expect(
    page.getByRole('region', { name: 'Recent turn costs' }).getByRole('link').first(),
  ).toContainText('partial')
  const attention = page.getByRole('region', { name: 'Attention row' })
  await expect(attention.getByRole('link', { name: /unpriced/ })).toHaveAttribute(
    'href',
    `/usage?session=${SEARCH_USAGE_SCENARIO_SESSION_ID}`,
  )
  await expect(attention.getByRole('button', { name: 'Example session' })).toBeEnabled()
  await page.screenshot({ path: testInfo.outputPath('cost-chips.png') })
  for (const width of [761, 390]) {
    await page.setViewportSize({ width, height: 844 })
    const sessionBox = await attention
      .getByRole('button', { name: 'Example session' })
      .boundingBox()
    const costBox = await attention.getByRole('link', { name: /unpriced/ }).boundingBox()
    if (!sessionBox || !costBox) throw new Error('Both Attention destinations must be visible')
    expect(costBox.y).toBeGreaterThanOrEqual(sessionBox.y + sessionBox.height)
    expect(await attention.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
      true,
    )
    await attention.screenshot({ path: testInfo.outputPath(`attention-cost-${width}.png`) })
  }
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
  await expect(session.getByRole('link')).toHaveText('Cost unavailable')
  unavailable = false
  await page.getByRole('button', { name: 'Refresh costs' }).click()
  await expect(session.getByRole('link')).toHaveText('$0')
})

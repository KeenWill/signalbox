import { webContractBootstrapFixture } from '../src/product.fixture'
import { SearchUsageScenarioSource } from '../src/search-usage/scenario'
import { expect, test } from './fontTest'

for (const available of [true, false]) {
  test(`product Usage ${available ? 'loads filtered server costs' : 'reports missing usage capability'}`, async ({
    page,
  }, testInfo) => {
    const source = new SearchUsageScenarioSource()
    const usageReads: URL[] = []
    let bootstrapReads = 0
    await page.route('**/api/**', async (route) => {
      const url = new URL(route.request().url())
      if (url.pathname === '/api/bootstrap') {
        bootstrapReads += 1
        return route.fulfill({
          json: {
            ...webContractBootstrapFixture,
            capabilities: {
              ...webContractBootstrapFixture.capabilities,
              bounded_usage_cost: available,
            },
          },
        })
      }
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      if (url.pathname === '/api/attention/follow')
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      usageReads.push(url)
      const filters = { modelId: url.searchParams.get('model_id') ?? undefined }
      if (url.pathname === '/api/usage/summary')
        return route.fulfill({ json: await source.usageSummary(filters) })
      if (url.pathname === '/api/usage/calls')
        return route.fulfill({
          json: await source.usageCalls({ filters, order: 'newest', maxItems: 100 }),
        })
      throw new Error(`Unexpected endpoint ${url.pathname}`)
    })
    await page.goto('/usage')
    await expect(page.getByRole('heading', { name: 'Usage', exact: true })).toBeVisible()
    await expect(page.getByRole('heading', { name: 'Usage unavailable' })).toHaveCount(0)
    if (!available) {
      await expect(page.getByRole('alert')).toContainText('Usage could not load')
      expect(usageReads).toHaveLength(0)
      expect(bootstrapReads).toBe(1)
      return
    }
    const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
    await expect(rows).toHaveAttribute('data-total-loaded', '100')
    await expect(rows).toContainText('unpriced')
    await page
      .getByLabel('Model', { exact: true })
      .selectOption('00000000-0000-0000-0000-000000001003')
    await expect(rows).toHaveAttribute('data-total-loaded', '48')
    expect(usageReads.some((url) => url.pathname === '/api/usage/summary')).toBe(true)
    expect(
      usageReads.some(
        (url) => url.searchParams.get('model_id') === '00000000-0000-0000-0000-000000001003',
      ),
    ).toBe(true)
    expect(bootstrapReads).toBe(1)
    await page.reload()
    await expect(rows).toHaveAttribute('data-total-loaded', '48')
    await page.setViewportSize({ width: 390, height: 844 })
    await expect(page.getByRole('columnheader', { name: 'Cost' })).toBeVisible()
    await page.screenshot({ path: testInfo.outputPath('product-usage-phone.png'), fullPage: true })
    for (const width of [390, 1280]) {
      await page.setViewportSize({ width, height: 600 })
      const disclosure = page.getByText('Session and turn costs in loaded calls', { exact: true })
      await disclosure.click()
      const subtotals = page.getByRole('region', { name: 'Loaded cost subtotals' })
      const lastTurn = subtotals.getByRole('button', { name: /^Turn / }).last()
      await lastTurn.focus()
      await expect(lastTurn).toBeInViewport()
      const usage = page.getByRole('region', { name: 'Usage', exact: true })
      expect(
        await usage.evaluate((element) => {
          const scroller = element.parentElement
          return (
            scroller !== null &&
            getComputedStyle(scroller).overflowY === 'auto' &&
            scroller.scrollHeight > scroller.clientHeight &&
            scroller.scrollTop > 0
          )
        }),
      ).toBe(true)
      await disclosure.click()
    }
  })
}

test('recovers Usage through the shell bootstrap retry without separate admission', async ({
  page,
}) => {
  let bootstrapReads = 0
  let usageReads = 0
  await page.route('**/api/**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === '/api/bootstrap') {
      bootstrapReads += 1
      return bootstrapReads === 1
        ? route.fulfill({ status: 503, body: 'Unavailable' })
        : route.fulfill({ json: webContractBootstrapFixture })
    }
    if (path.startsWith('/api/attention'))
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    usageReads += 1
    return route.fulfill({
      json:
        path === '/api/usage/summary'
          ? { groups: [], truncated: false }
          : { calls: [], continuation: null },
    })
  })
  await page.goto('/usage')
  await expect(page.getByRole('button', { name: 'Retry connection' })).toBeVisible()
  expect(bootstrapReads).toBe(1)
  expect(usageReads).toBe(0)
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await expect(page.getByText('No calls match these filters.')).toBeVisible()
  expect(bootstrapReads).toBe(2)
  expect(usageReads).toBe(2)
})

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

test('never presents cached scenario usage as server usage during failed reads', async ({
  page,
}) => {
  const { webContractBootstrapFixture } = await import('../product.fixture')
  let release: () => void = () => undefined
  const pending = new Promise<void>((resolve) => {
    release = resolve
  })
  let usageReads = 0
  await page.route('**/api/**', async (route) => {
    if (new URL(route.request().url()).pathname === '/api/bootstrap')
      return route.fulfill({ json: webContractBootstrapFixture })
    usageReads += 1
    await pending
    await route.fulfill({ status: 503, body: 'Usage unavailable' })
  })
  await page.goto('/src/search-usage/preview.html?workbench')
  const rows = page.getByRole('rowgroup', { name: 'Usage call rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', '100')
  await expect(rows).toContainText('unpriced')
  await page.getByRole('button', { name: 'Load server usage' }).click()
  await expect.poll(() => usageReads).toBe(2)
  await expect(rows).toHaveAttribute('data-total-loaded', '0')
  await expect(page.getByText('unpriced', { exact: false })).toHaveCount(0)
  release()
  await expect(page.getByRole('alert')).toContainText('Usage could not load')
  await expect(rows).toHaveAttribute('data-total-loaded', '0')
  await expect(page.getByText('unpriced', { exact: false })).toHaveCount(0)
})

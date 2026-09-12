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

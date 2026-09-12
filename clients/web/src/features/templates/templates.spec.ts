import { expect, test } from '@playwright/test'
import { detailFixture, templateFixture } from './fixtures'

const scenario = '/src/features/templates/scenario.html'
test.beforeEach(async ({ page }) => {
  await page.route('**/api/templates', (route) =>
    route.fulfill({ json: { templates: [templateFixture] } }),
  )
  await page.route('**/api/templates/code-review', (route) =>
    route.fulfill({ json: detailFixture }),
  )
})

test('opens the definition using the keyboard', async ({ page }, testInfo) => {
  await page.goto(scenario)
  const row = page.getByRole('button', { name: /code-review/ })
  await expect(row).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('template-list.png'), fullPage: true })
  await row.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Start session' })).toBeDisabled()
  await page.getByText('System instructions', { exact: true }).click()
  await expect(page.getByText(detailFixture.system_prompt, { exact: true })).toBeVisible()
  await page.getByText('Definition and digest', { exact: true }).click()
  await expect(page.getByText(templateFixture.digest, { exact: true })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('template-detail.png'), fullPage: true })
  await page.getByRole('button', { name: 'Back to templates' }).click()
  await expect(row).toBeVisible()
  await expect(row).toBeFocused()
})

test('fits a phone viewport', async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(scenario)
  await page.getByRole('button', { name: /code-review/ }).click()
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('template-phone.png'), fullPage: true })
})

test('virtualizes a large loaded catalog', async ({ page }) => {
  const templates = Array.from({ length: 10000 }, (_, index) => ({
    ...templateFixture,
    name: `template-${index}`,
  }))
  await page.route('**/api/templates', (route) => route.fulfill({ json: { templates } }))
  await page.goto(scenario)
  await expect(page.getByRole('button', { name: /template-0 / })).toBeVisible()
  expect(await page.getByRole('button').count()).toBeLessThan(40)
})

test('restores list focus after browser Back', async ({ page }) => {
  await page.goto(scenario)
  const row = page.getByRole('button', { name: /code-review/ })
  await row.focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/templates#code-review$/)
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await page.goBack()
  await expect(row).toBeFocused()
  await page.goForward()
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await page.goBack()
  await expect(row).toBeFocused()
})

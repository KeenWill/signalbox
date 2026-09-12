import { expect, test } from '@playwright/test'

test('tool summaries expose raw evidence by keyboard', async ({ page }, testInfo) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/src/features/tools/scenario.html')
  await expect(page.getByText('Exit 0')).toBeVisible()
  const unknown = page.getByRole('article', { name: 'Tool custom_tool', exact: true })
  await expect(unknown.getByText('Greeting')).toBeVisible()
  const raw = unknown.getByRole('button', { name: 'Raw' })
  await raw.focus()
  await page.keyboard.press('Enter')
  await expect(raw).toHaveAttribute('aria-expanded', 'true')
  await expect(unknown.getByText('{"greeting":"Hello"}')).toBeVisible()
  await page.keyboard.press('Enter')
  await expect(raw).toHaveAttribute('aria-expanded', 'false')
  await expect(page.getByRole('link', { name: 'Rust documentation' })).toHaveAttribute(
    'href',
    'https://doc.rust-lang.org/',
  )
  await page.screenshot({ path: testInfo.outputPath('tool-renderers.png'), fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByText('Exit 0')).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('tool-renderers-phone.png'), fullPage: true })
  expect(errors).toEqual([])
})

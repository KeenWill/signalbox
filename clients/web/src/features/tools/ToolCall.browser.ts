import { expect, test } from '@playwright/test'
import { fileEvidence, rawFileEvidence } from './toolScenario'

test('tool summaries expose raw evidence by keyboard', async ({ page }, testInfo) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/src/features/tools/scenario.html')
  await expect(page.getByText('Exit 0')).toBeVisible()
  const file = page.getByRole('article', { name: 'Tool read_file', exact: true }).last()
  expect(
    await file.getByRole('region', { name: 'File contents' }).locator('code').textContent(),
  ).toBe(fileEvidence)
  await file.getByRole('button', { name: 'Raw', exact: true }).click()
  expect(
    await file.getByRole('region', { name: 'Output', exact: true }).locator('code').textContent(),
  ).toBe(rawFileEvidence)
  await file.getByRole('button', { name: 'Raw', exact: true }).click()
  const proposedDiff = page
    .getByRole('region', { name: 'Proposed changes', exact: true })
    .locator('code')
  expect(await proposedDiff.textContent()).toBe('- Hello\n+ Welcome')
  expect(await proposedDiff.innerText()).toBe('- Hello\n+ Welcome')
  const gitDiff = page.getByRole('region', { name: 'Diff', exact: true }).locator('code')
  expect(await gitDiff.textContent()).toBe('-before\n+after\n')
  expect(await gitDiff.innerText()).toBe('-before\n+after\n')
  const unknown = page.getByRole('article', { name: 'Tool custom_tool', exact: true }).first()
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

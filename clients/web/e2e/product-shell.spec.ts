import { expect, type Page, test } from '@playwright/test'
import bootstrapFixture from '../src/generated/web-contract-bootstrap.json' with { type: 'json' }

const emptySessionPage = {
  continuation: null,
  cursor: '0',
  sort: 'last_activity_descending',
  summaries: [],
  total: '0',
} as const

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) => route.fulfill({ json: emptySessionPage }))
}

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const platformModifier = (page: Page) =>
  page.evaluate(() => (/Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control'))

test('opens the product at Attention with generated-contract transport status', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
  await expect(page.getByText('signalbox.web-http · 1')).toBeVisible()
  await expect(page.getByRole('link', { name: /Attention/ })).toHaveAttribute(
    'aria-current',
    'page',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('navigates from Attention to Sessions with the shared semantic link', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('heading', { name: 'Sessions', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus out of desktop navigation before hiding it in focus layout', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).focus()
  await page.keyboard.press('Shift+W')

  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('completes route switching from the command palette without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus after sequence-driven product navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('g')
  await page.keyboard.press('s')

  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps product sequences suspended while the command palette is open', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await expect(palette.getByRole('button', { name: /Open command palette/ })).toHaveCount(0)
  const theme = await page.evaluate(() => document.documentElement.dataset.theme)
  await page.keyboard.press('Shift+T')
  await expect.poll(() => page.evaluate(() => document.documentElement.dataset.theme)).toBe(theme)
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(palette).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('Escape closes only the active palette over a selected session', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
  await page.goto(`/sessions?session=${sessionId}`)

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(sessionId)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('clears scenario-owned help when browser history returns to the product', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/scenario\/streaming/)

  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
  await page.keyboard.press('Shift+/')
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.goBack()
  await expect(page).toHaveURL(/attention/)
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/sessions/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus into the scenario workspace after product navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Scenario studio/ }).click()

  await expect(page).toHaveURL(/scenario\/streaming/)
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses a navigation sheet on a phone viewport and unwinds it with Escape', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  const openNavigation = page.getByRole('button', { name: 'Open navigation' })
  await openNavigation.click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(openNavigation).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores visible focus after desktop navigation closes', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open navigation/ }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'

const emptySessionPage = {
  continuation: null,
  cursor: '0',
  sort: 'last_activity_descending',
  summaries: [],
  total: '0',
} as const

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
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
  await expect(
    page.getByText(
      `${webContractBootstrapFixture.contract.name} · ${webContractBootstrapFixture.contract.version}`,
    ),
  ).toBeVisible()
  await expect(page).toHaveTitle('Attention · Signalbox')
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

test('describes Settings as browser-local rather than daemon-backed', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/settings')

  await expect(
    page.getByRole('heading', { name: 'Local settings are not exposed in this slice' }),
  ).toBeVisible()
  await expect(page.getByText(/do not depend on a daemon read contract/)).toBeVisible()
  await expect(page.getByRole('status')).toHaveText('Browser-local preferences')
  await expect(page.getByText('Transport unavailable', { exact: true })).toHaveCount(0)
  await expect(page.getByText('Incompatible daemon contract', { exact: true })).toHaveCount(0)
  await expect(
    page.getByText('Operational data is not exposed by this daemon contract'),
  ).toHaveCount(0)
  const inspector = page.getByRole('complementary', { name: 'Inspector' })
  await expect(inspector.getByText('Browser', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Local preferences', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Daemon', { exact: true })).toHaveCount(0)
  await expect(
    inspector.getByText('Presentation preferences are stored locally in this browser.'),
  ).toBeVisible()
  await expect(inspector.getByText(/server-provided evidence/)).toHaveCount(0)
  const settingsCopy = page.getByRole('heading', {
    name: 'Local settings are not exposed in this slice',
  })
  expect((await settingsCopy.boundingBox())?.width).toBeGreaterThan(200)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('clears scenario-only help when browser history returns to the product shell', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/\/scenario\/streaming$/)

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.goBack()

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves a scenario-specific title after leaving the product shell', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const scenarioEntry = page.getByRole('link', { name: /Scenario studio/ })
  await scenarioEntry.focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page).toHaveTitle('Streaming session · Signalbox scenarios')
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeFocused()
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

test('returns focus to a visible desktop control after closing palette-opened navigation', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open navigation/ }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('button', { name: 'Open command palette' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses the displayed product navigation sequence', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page).toHaveTitle('Sessions · Signalbox')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus before focus layout hides product navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).focus()
  await page.keyboard.press('Shift+W')

  await expect(page.getByRole('main')).toBeFocused()
  await expect(page.getByRole('navigation', { name: 'Product' })).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends product hotkeys while the command palette owns input', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(palette).toBeVisible()
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

test('closes phone navigation after selecting a route', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(navigation).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

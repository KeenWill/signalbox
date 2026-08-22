import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: webContractBootstrapFixture }))

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
  await page.goto('/settings')

  await expect(page.getByRole('heading', { name: 'Operator preferences' })).toBeVisible()
  await expect(page.getByText(/Presentation choices stay in this browser/)).toBeVisible()
  await expect(page.getByText('Browser-local preferences', { exact: true })).toBeVisible()
  await expect(
    page.getByText('Operational data is not exposed by this daemon contract'),
  ).toHaveCount(0)
  const inspector = page.getByRole('complementary', { name: 'Inspector' })
  await expect(inspector.getByText('Browser', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Local preferences', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Daemon', { exact: true })).toHaveCount(0)
  await expect(
    inspector.getByText(
      'Presentation preferences are stored locally in this browser and do not represent server evidence.',
    ),
  ).toBeVisible()
  await expect(inspector.getByText(/server-provided evidence/)).toHaveCount(0)
  const settingsCopy = page.getByRole('heading', { name: 'Operator preferences' })
  expect((await settingsCopy.boundingBox())?.width).toBeGreaterThan(200)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves a scenario-specific title after leaving the product shell', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page).toHaveTitle('Streaming session · Signalbox scenarios')
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

test('changes and restores a Settings preference without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto(settingsPreferenceFixture.path)

  const lightTheme = page.getByRole('radio', { name: settingsPreferenceFixture.changedTheme })
  await lightTheme.focus()
  await page.keyboard.press('Space')
  await expect(lightTheme).toBeChecked()
  await page.reload()
  await expect(
    page.getByRole('radio', { name: settingsPreferenceFixture.changedTheme }),
  ).toBeChecked()
  await page.getByRole('button', { name: settingsPreferenceFixture.restoreAction }).focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('radio', { name: settingsPreferenceFixture.defaultTheme }),
  ).toBeChecked()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies saved pane widths to the scenario workspace', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/settings')

  const paneSliders = page.getByRole('group', { name: 'Workbench panes' }).getByRole('slider')
  await paneSliders.nth(0).fill('300')
  await paneSliders.nth(1).fill('400')
  await page.setViewportSize({ width: 1000, height: 800 })
  await expect(page.locator('.product-navigation-pane')).toHaveCSS('width', '300px')
  await page.getByRole('link', { name: /Scenario studio/ }).click()

  await expect(page.locator('.navigation-pane')).toHaveCSS('width', '300px')
  await expect(page.getByRole('complementary', { name: 'Diagnostics' })).toBeHidden()
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(page.getByRole('complementary', { name: 'Diagnostics' })).toHaveCSS('width', '400px')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps Settings available without consulting daemon bootstrap', async ({ page }) => {
  const problems = watchBrowser(page)
  let bootstrapRequests = 0
  await page.route('**/api/bootstrap', (route) => {
    bootstrapRequests += 1
    return route.abort()
  })

  await page.goto('/settings')

  await expect(page.getByRole('heading', { name: 'Settings', level: 1 })).toBeVisible()
  await expect(page.getByText('Browser-local preferences', { exact: true })).toBeVisible()
  await expect(page.getByText('Transport unavailable')).toHaveCount(0)
  expect(bootstrapRequests).toBe(0)
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

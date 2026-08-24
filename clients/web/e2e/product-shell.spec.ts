import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { useDeterministicImportApi } from './import-api-fixture'

// Item charges follow the wire contract: a 64-byte envelope plus the UTF-8 event-kind spelling
// (all three kinds below spell 14 bytes), so each item projects 78 bytes and the window 234.
const sessionWorkspaceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  firstAddress: '41',
  latestAddress: '43',
  itemCount: '3',
  itemBytes: 78,
  projectedBytes: 234,
} as const

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const importsProductFixture = {
  path: '/imports',
  loadedImports: '100',
  latestLoadedPosition: '51',
} as const

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: webContractBootstrapFixture }))

const useDeterministicSession = async (page: Page) => {
  await page.route('**/api/sessions/**', (route) => {
    if (new URL(route.request().url()).pathname.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionWorkspaceFixture.id,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: sessionWorkspaceFixture.itemBytes,
            },
            {
              address: { event_sequence: '42' },
              kind: 'turn_activated',
              projected_structured_bytes: sessionWorkspaceFixture.itemBytes,
            },
            {
              address: { event_sequence: '43' },
              kind: 'turn_completed',
              projected_structured_bytes: sessionWorkspaceFixture.itemBytes,
            },
          ],
          projected_structured_bytes: sessionWorkspaceFixture.projectedBytes,
          continuation_before: { event_sequence: sessionWorkspaceFixture.firstAddress },
          continuation_after: null,
        },
      })
    }
    return route.fulfill({
      json: {
        session_id: sessionWorkspaceFixture.id,
        sizes: {
          item_count: sessionWorkspaceFixture.itemCount,
          projected_text_bytes: '0',
          projected_structured_bytes: String(sessionWorkspaceFixture.projectedBytes),
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: sessionWorkspaceFixture.firstAddress },
        latest_address: { event_sequence: sessionWorkspaceFixture.latestAddress },
        work: { active_turn_count: '1', queued_turn_count: '2' },
        observed_through: sessionWorkspaceFixture.latestAddress,
      },
    })
  })
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

  await expect(page.getByRole('heading', { name: 'Operator preferences' })).toBeVisible()
  await expect(
    page.getByText(/do not change daemon authority or manufacture operational facts/),
  ).toBeVisible()
  await expect(page.locator('.contract-state')).toHaveText('Browser-local preferences')
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
  const settingsCopy = page.getByRole('heading', { name: 'Operator preferences' })
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
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens visible keyboard help and follows product navigation sequences', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('Shift+/')
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await page.keyboard.press('Escape')
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.locator('main.product-main')).toBeFocused()
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

test('opens and inspects a bounded production session without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await expect(palette.getByRole('button', { name: /Select first loaded item/ })).toHaveCount(0)
  await expect(palette.getByRole('button', { name: /Select latest loaded item/ })).toHaveCount(0)
  await page.keyboard.press('Escape')
  await expect(palette).toBeHidden()

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(page.getByText(sessionWorkspaceFixture.itemCount, { exact: true })).toBeVisible()
  const accepted = page.getByRole('button', { name: /41 input accepted/ })
  const acceptedItem = page.getByRole('listitem').filter({ has: accepted })
  await accepted.focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('region', { name: 'transcript attachments unavailable' }),
  ).toBeVisible()
  await expect(page.getByRole('region', { name: 'composer attachments unavailable' })).toBeVisible()
  const completed = page.getByRole('button', { name: /43 turn completed/ })
  await completed.focus()
  await page.keyboard.press('Enter')
  await expect(completed).toHaveAttribute('aria-expanded', 'true')
  const completedItem = page.getByRole('listitem').filter({ has: completed })
  await expect(
    completedItem.getByText('Header only; rich event detail is not exposed'),
  ).toBeVisible()
  await expect(page.getByRole('button', { name: 'Previous window' })).toBeEnabled()
  await expect(page.getByRole('button', { name: 'Next window' })).toBeDisabled()
  const firstWindowRequest = page.waitForRequest((request) => {
    const url = new URL(request.url())
    return url.pathname.endsWith('/timeline') && url.searchParams.get('anchor') === 'first'
  })
  await page.keyboard.press('g')
  await page.keyboard.press('g')
  await firstWindowRequest
  await expect(acceptedItem).toHaveClass(/selected/)

  const latestWindowRequest = page.waitForRequest((request) => {
    const url = new URL(request.url())
    return url.pathname.endsWith('/timeline') && url.searchParams.get('anchor') === 'latest'
  })
  await page.keyboard.press('Shift+G')
  await latestWindowRequest
  await expect(completedItem).toHaveClass(/selected/)

  const reopenRequest = page.waitForRequest((request) =>
    new URL(request.url()).pathname.endsWith(`/api/sessions/${sessionWorkspaceFixture.id}`),
  )
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await reopenRequest
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('operates the bounded Imports surface and leaves through one command palette', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  const importRows = page.getByRole('rowgroup', { name: 'Imported conversation rows' })
  await expect(importRows).toHaveAttribute('data-total-loaded', importsProductFixture.loadedImports)
  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await entries.focus()
  await entries.press('End')
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    importsProductFixture.latestLoadedPosition,
  )

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toHaveCount(1)
  await page.getByRole('button', { name: /Go to Settings/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/settings$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

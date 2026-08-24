import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'

const sessionWorkspaceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  firstAddress: '41',
  latestAddress: '43',
  itemCount: '3',
  projectedBytes: 234,
} as const

const sessionCatalogSummary = {
  action: null,
  active_turn_count: '1',
  archived: false,
  current_turn_id: '00000000-0000-0000-0000-000000000041',
  goal_block: null,
  judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
  last_activity: { kind: 'turn', unix_microseconds: '1787400000000000' },
  queued_turn_count: '2',
  session_id: sessionWorkspaceFixture.id,
  state: 'active',
  title_summary: 'Release train session',
  title_truncated: false,
} as const

const sessionLiveSnapshot = {
  active: {
    state: { kind: 'running', model_call_id: '00000000-0000-0000-0000-000000000042' },
    turn_id: '00000000-0000-0000-0000-000000000041',
  },
  observed_through: sessionWorkspaceFixture.latestAddress,
  queued_turn_count: '2',
  queued_turn_ids: ['00000000-0000-0000-0000-000000000051', '00000000-0000-0000-0000-000000000052'],
  reconciliation: null,
  runner: {
    connection_health: 'connected',
    placement_revision: '7',
    runner_id: '00000000-0000-0000-0000-000000000061',
    state: 'pinned',
  },
  session_id: sessionWorkspaceFixture.id,
} as const

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/sessions**', (route) =>
    new URL(route.request().url()).pathname === '/api/sessions'
      ? route.fulfill({
          json: {
            continuation: null,
            cursor: '0',
            sort: 'last_activity_descending',
            summaries: [],
            total: '0',
          },
        })
      : route.fallback(),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '0',
          sort: 'last_activity_descending',
          summaries: [],
          total: '0',
        },
      })}\n`,
    }),
  )
}

const useDeterministicSession = async (page: Page) => {
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: sessionWorkspaceFixture.latestAddress,
          sort: 'last_activity_descending',
          summaries: [sessionCatalogSummary],
          total: '1000',
        },
      })}\n`,
    }),
  )
  await page.route('**/api/sessions**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    if (pathname === '/api/sessions') {
      return route.fulfill({
        json: {
          continuation: null,
          cursor: sessionWorkspaceFixture.latestAddress,
          sort: 'last_activity_descending',
          summaries: [sessionCatalogSummary],
          total: '1000',
        },
      })
    }
    if (pathname.endsWith('/follow')) {
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: sessionLiveSnapshot })}\n`,
      })
    }
    if (pathname.endsWith('/live')) return route.fulfill({ json: sessionLiveSnapshot })
    if (pathname.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionWorkspaceFixture.id,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '42' },
              kind: 'turn_activated',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '43' },
              kind: 'turn_completed',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: sessionWorkspaceFixture.projectedBytes,
          continuation_before: null,
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
  await expect(page).toHaveTitle('Attention · Signalbox')
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
  await expect(page).toHaveTitle('Sessions · Signalbox')
  await expect(page.getByRole('heading', { name: 'Sessions', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('describes Settings as browser-local rather than daemon-backed', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/settings')

  await expect(page.getByRole('heading', { name: 'Operator preferences' })).toBeVisible()
  await expect(page.getByText(/Presentation choices stay in this browser/)).toBeVisible()
  await expect(page.getByText('Browser-local preferences', { exact: true })).toHaveAttribute(
    'role',
    'status',
  )
  await expect(page.getByText('Transport unavailable', { exact: true })).toHaveCount(0)
  await expect(page.getByText('Incompatible daemon contract', { exact: true })).toHaveCount(0)
  await expect(
    page.getByText('Operational data is not exposed by this daemon contract'),
  ).toHaveCount(0)
  const inspector = page.getByRole('complementary', { name: 'Inspector' })
  await expect(inspector.getByText('Browser', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Local settings', { exact: true })).toBeVisible()
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

test('gates Sessions on the validated bootstrap capability', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...webContractBootstrapFixture,
        capabilities: {
          ...webContractBootstrapFixture.capabilities,
          bounded_session_timeline: false,
        },
      },
    }),
  )
  await page.goto('/sessions')

  await expect(page.getByText('Session reads unavailable')).toBeVisible()
  await expect(page.getByRole('listbox', { name: 'Sessions' })).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

const routeBootstrapRecovery = async (page: Page) => {
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => {
    attempts += 1
    return attempts === 1
      ? route.fulfill({ status: 503, body: 'temporarily unavailable' })
      : route.fulfill({ json: webContractBootstrapFixture })
  })
  return () => attempts
}

test('retries a failed product bootstrap after the daemon recovers', async ({ page }) => {
  const problems = watchBrowser(page)
  const bootstrapAttempts = await routeBootstrapRecovery(page)
  await page.route('**/api/sessions**', (route) =>
    new URL(route.request().url()).pathname === '/api/sessions'
      ? route.fulfill({
          json: {
            continuation: null,
            cursor: '0',
            sort: 'last_activity_descending',
            summaries: [],
            total: '0',
          },
        })
      : route.fallback(),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '0',
          sort: 'last_activity_descending',
          summaries: [],
          total: '0',
        },
      })}\n`,
    }),
  )
  await page.goto('/sessions')

  await expect(page.getByText('Transport unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry contract' }).click()

  await expect(page.getByText('Session reads available')).toBeVisible()
  await expect(
    page.getByText(
      `${webContractBootstrapFixture.contract.name} · ${webContractBootstrapFixture.contract.version}`,
    ),
  ).toBeVisible()
  expect(bootstrapAttempts()).toBe(2)
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every((message) =>
      message.includes('Failed to load resource: the server responded with a status of 503'),
    ),
  ).toBe(true)
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
  await expect(page.getByRole('group', { name: 'Remote media' })).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens and inspects a bounded production session without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessions = page.getByRole('listbox', { name: 'Sessions' })
  await sessions.focus()
  await sessions.press('Enter')
  await expect(page.getByRole('heading', { name: 'Release train session' })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(page.getByText(sessionWorkspaceFixture.itemCount, { exact: true })).toBeVisible()
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(page.getByRole('option', { name: /41 input accepted/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  await expect(timeline).toHaveAttribute('aria-activedescendant', 'session-timeline-option-41')
  const latest = page.getByRole('button', { name: /Latest/ })
  await latest.focus()
  await page.keyboard.press('Tab')
  await expect(timeline).toBeFocused()
  await latest.focus()
  await page.keyboard.press('j')
  await expect(timeline).toBeFocused()
  await expect(page.getByRole('option', { name: /42 turn activated/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )

  const completed = page.getByRole('option', { name: /43 turn completed/ })
  await completed.click()
  await expect(timeline).toBeFocused()
  await expect(completed).toHaveAttribute('aria-controls', 'session-timeline-detail-43')
  await expect(completed).toHaveAttribute('aria-describedby', 'session-timeline-disclosure-43')
  await expect(page.locator('#session-timeline-disclosure-43')).toHaveText('Expanded')
  await expect(page.locator('#session-timeline-detail-43')).toBeVisible()
  await expect(page.getByText('Header only; rich event detail is not exposed')).toBeVisible()
  const inspector = page.getByLabel('Inspector')
  await expect(inspector.getByText(sessionWorkspaceFixture.id, { exact: true })).toBeVisible()
  await expect(inspector.getByText('43', { exact: true })).toBeVisible()
  await expect(inspector.getByText('turn completed', { exact: true })).toBeVisible()
  await expect(inspector.getByText('78', { exact: true })).toBeVisible()

  await page.getByRole('button', { name: /First/ }).click()
  await expect(page.getByRole('option', { name: /41 input accepted/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  await latest.click()
  await expect(completed).toHaveAttribute('aria-selected', 'true')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gives Full and Condensed distinct Session presentations', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  await page.getByRole('option', { name: /Release train session/ }).click()
  await expect(page.getByRole('heading', { name: 'Release train session' })).toBeVisible()
  await expect(page.locator('.session-item-summary small').first()).toBeHidden()

  await page.getByRole('link', { name: /Settings/ }).click()
  await page.getByRole('radio', { name: 'Full' }).check()
  await page.getByRole('link', { name: /Sessions/ }).click()
  await page.getByRole('option', { name: /Release train session/ }).click()

  await expect(page.locator('.session-item-summary small').first()).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps maximum pane widths inside the viewport', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 1180, height: 800 })
  await page.goto('/settings')

  const paneWidths = page.locator('.pane-preferences input[type="range"]')
  await paneWidths.nth(0).fill('360')
  await paneWidths.nth(1).fill('480')

  await expect(page.locator('.product-inspector')).toBeHidden()
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(1180)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('honors the saved navigation width below 1080px', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 900, height: 800 })
  await page.goto('/settings')

  const navigationWidth = page.locator('.pane-preferences input[type="range"]').first()
  await navigationWidth.fill('320')

  await expect(page.locator('.product-navigation-pane')).toHaveCSS('width', '320px')
  await expect(page.getByText('320px', { exact: true })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

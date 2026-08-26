import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { useDeterministicImportApi } from './import-api-fixture'

const importsProductFixture = {
  path: '/imports',
  loadedImports: '100',
  latestLoadedPosition: '51',
} as const

const sessionWorkspaceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  firstAddress: '41',
  latestAddress: '43',
  itemCount: '3',
  projectedBytes: 234,
} as const

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

const useRecoveringBootstrap = async (page: Page) => {
  const state = { unavailable: true, attempts: 0 }
  await page.route('**/api/bootstrap', (route) => {
    state.attempts += 1
    return state.unavailable
      ? route.fulfill({ status: 503, body: 'temporarily unavailable' })
      : route.fulfill({ json: bootstrapFixture })
  })
  return { recover: () => (state.unavailable = false), attempts: () => state.attempts }
}

// Playwright matches route handlers most-recently-registered first and retires a `times: 1`
// handler after its single use, so the transport refuses the first admission and serves the
// deterministic bootstrap on every retry. The sequence lives here so a test body reads as
// straight-line code instead of branching on an attempt counter.
const useBootstrapRecoveringAfterOneOutage = async (page: Page) => {
  const admission = { attempts: 0 }
  await page.route('**/api/bootstrap', (route) => {
    admission.attempts += 1
    return route.fulfill({ json: bootstrapFixture })
  })
  await page.route(
    '**/api/bootstrap',
    (route) => {
      admission.attempts += 1
      return route.fulfill({ status: 503, body: 'temporarily unavailable' })
    },
    { times: 1 },
  )
  return admission
}

const useDeterministicSession = async (
  page: Page,
  shouldFailTimeline: (sessionId: string) => boolean = () => false,
  timelineKind: (
    sessionId: string,
    address: string,
  ) => 'input_accepted' | 'turn_activated' | 'turn_completed' | 'turn_cancelled' | undefined = () =>
    undefined,
) => {
  await page.route('**/api/sessions/**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    const requestedSessionId = decodeURIComponent(pathname.split('/')[3] ?? '')
    if (pathname.endsWith('/timeline')) {
      if (shouldFailTimeline(requestedSessionId)) {
        return route.fulfill({ status: 503, body: 'temporarily unavailable' })
      }
      return route.fulfill({
        json: {
          session_id: requestedSessionId,
          items: [
            {
              address: { event_sequence: '41' },
              kind: timelineKind(requestedSessionId, '41') ?? 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '42' },
              kind: timelineKind(requestedSessionId, '42') ?? 'turn_activated',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '43' },
              kind: timelineKind(requestedSessionId, '43') ?? 'turn_completed',
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
        session_id: requestedSessionId,
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
  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
  await expect(page.getByRole('link', { name: /Attention/ })).toHaveAttribute(
    'aria-current',
    'page',
  )
  const sessionsLink = page.getByRole('link', { name: /Sessions/ })
  await sessionsLink.focus()
  await page.keyboard.press('j')
  await expect(sessionsLink).toBeFocused()
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

test('restores the scenario title after leaving product routes', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await expect(page).toHaveTitle('Attention · Signalbox')

  await page.getByRole('link', { name: /Scenario studio/ }).click()

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page).toHaveTitle('Streaming session · Signalbox scenarios')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gates Sessions on the validated bootstrap capability', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_session_timeline: false },
      },
    }),
  )
  await page.goto('/sessions')

  await expect(page.getByText('Timeline reads unavailable')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open workspace' })).toBeDisabled()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gates Sessions on valid timeline limits', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_timeline_window_items: 257 },
      },
    }),
  )
  await page.goto('/sessions')

  await expect(page.getByText('Timeline reads unavailable')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open workspace' })).toBeDisabled()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('leaves focus in place when Escape has no surface to unwind', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const sessionsLink = page.getByRole('link', { name: /Sessions/ })
  await sessionsLink.focus()
  await page.keyboard.press('Escape')

  await expect(sessionsLink).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('retries a failed product bootstrap after the daemon recovers', async ({ page }) => {
  const problems = watchBrowser(page)
  const scenario = await useRecoveringBootstrap(page)
  await page.goto('/sessions')

  await expect(page.getByText('Bootstrap unavailable')).toBeVisible()
  scenario.recover()
  await page.getByRole('button', { name: 'Retry bootstrap' }).click()

  await expect(page.getByText('Timeline reads available')).toBeVisible()
  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
  expect(scenario.attempts()).toBe(2)
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

test('uses a navigation sheet on a phone viewport with a semantic close control', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  const openNavigation = page.getByRole('button', { name: 'Open navigation' })
  await openNavigation.click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.getByRole('button', { name: 'Close navigation' }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(openNavigation).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns focus to the desktop command that opened product navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const openPalette = page.getByRole('button', { name: 'Open command palette' })
  await openPalette.click()
  await page.getByRole('button', { name: /Open product navigation/ }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.getByRole('button', { name: 'Close navigation' }).click()

  await expect(openPalette).toBeFocused()
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

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(page.getByText(sessionWorkspaceFixture.itemCount, { exact: true })).toBeVisible()
  await expect(page.getByRole('region', { name: 'composer attachments unavailable' })).toBeVisible()
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

  const accepted = page.getByRole('option', { name: /41 input accepted/ })
  await accepted.click()
  await expect(page.locator('#session-timeline-detail-41')).toBeVisible()
  await expect(
    page.getByRole('region', { name: 'transcript attachments unavailable' }),
  ).toBeVisible()
  await accepted.click()
  await expect(page.locator('#session-timeline-detail-41')).toBeHidden()

  await page.getByRole('button', { name: /First/ }).click()
  await expect(timeline).toBeFocused()
  await expect(page.getByRole('option', { name: /41 input accepted/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  await latest.click()
  await expect(timeline).toBeFocused()
  await expect(completed).toHaveAttribute('aria-selected', 'true')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gives Full and Condensed distinct Session presentations', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.locator('.session-item-summary small').first()).toBeHidden()

  await page.getByRole('link', { name: /Settings/ }).click()
  await page.getByRole('radio', { name: 'Full' }).check()
  await page.getByRole('link', { name: /Sessions/ }).click()
  const reopenedSessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await reopenedSessionId.fill(sessionWorkspaceFixture.id)
  await reopenedSessionId.press('Enter')

  await expect(page.locator('.session-item-summary small').first()).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps palette selection commands focused on the Session timeline', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Select next timeline item/ }).click()

  await expect(timeline).toBeFocused()
  await expect(page.getByRole('option', { name: /42 turn activated/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves the saved row when reopening the current Session fails', async ({ page }) => {
  const problems = watchBrowser(page)
  let failTimeline = false
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page, () => failTimeline)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('option', { name: /43 turn completed/ }).click()

  failTimeline = true
  await sessionId.press('Enter')
  await expect(page.getByRole('alert')).toBeVisible()

  const savedPosition = await page.evaluate(
    ({ key, id }) => {
      const stored = JSON.parse(localStorage.getItem(key) ?? '{}') as {
        lastLogicalPositions?: Record<string, string>
      }
      return stored.lastLogicalPositions?.[id]
    },
    { key: 'signalbox.web.preferences.v1', id: sessionWorkspaceFixture.id },
  )
  expect(savedPosition).toBe('43')
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every((message) =>
      message.includes('Failed to load resource: the server responded with a status of 503'),
    ),
  ).toBe(true)
})

test('preserves the saved row when revisiting a cached Session fails', async ({ page }) => {
  const problems = watchBrowser(page)
  const otherSessionId = '00000000-0000-0000-0000-000000000992'
  let failRevisitedSession = false
  await useDeterministicBootstrap(page)
  await useDeterministicSession(
    page,
    (sessionId) => failRevisitedSession && sessionId === sessionWorkspaceFixture.id,
  )
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('option', { name: /43 turn completed/ }).click()

  await sessionId.fill(otherSessionId)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: otherSessionId })).toBeVisible()

  failRevisitedSession = true
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('alert')).toBeVisible()

  const savedPosition = await page.evaluate(
    ({ key, id }) => {
      const stored = JSON.parse(localStorage.getItem(key) ?? '{}') as {
        lastLogicalPositions?: Record<string, string>
      }
      return stored.lastLogicalPositions?.[id]
    },
    { key: 'signalbox.web.preferences.v1', id: sessionWorkspaceFixture.id },
  )
  expect(savedPosition).toBe('43')
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every((message) =>
      message.includes('Failed to load resource: the server responded with a status of 503'),
    ),
  ).toBe(true)
})

test('clears cached Session projections after a refetch error', async ({ page }) => {
  const problems = watchBrowser(page)
  let failTimeline = false
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page, () => failTimeline)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()

  failTimeline = true
  await page.getByRole('button', { name: /Latest/ }).click()

  await expect(page.getByRole('alert')).toContainText(
    'The daemon could not provide this bounded session window',
  )
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toHaveCount(0)
  await expect(page.getByLabel('Inspector').getByText(sessionWorkspaceFixture.id)).toHaveCount(0)
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every((message) =>
      message.includes('Failed to load resource: the server responded with a status of 503'),
    ),
  ).toBe(true)
})

test('rejects conflicting retained Session evidence after a boundary refetch', async ({ page }) => {
  const problems = watchBrowser(page)
  let contradictRetainedEvent = false
  await useDeterministicBootstrap(page)
  await useDeterministicSession(
    page,
    () => false,
    (_sessionId, address) =>
      contradictRetainedEvent && address === '43' ? 'turn_cancelled' : undefined,
  )
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('option', { name: /43 turn completed/ })).toBeVisible()

  contradictRetainedEvent = true
  await page.getByRole('button', { name: /Latest/ }).click()

  await expect(page.getByRole('alert')).toContainText(
    'timeline source returned conflicting data for a retained address',
  )
  expect(problems.pageErrors).toEqual([])
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

test('mounts Imports inside the product shell without a second navigation or header', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  await expect(page.getByRole('heading', { name: 'Imports', level: 1 })).toBeVisible()
  await expect(page.locator('.product-shell')).toHaveCount(1)
  await expect(page.locator('.imports-shell-product')).toHaveCount(1)
  await expect(page.locator('.imports-navigation')).toHaveCount(0)
  await expect(page.locator('.imports-header')).toHaveCount(0)
  await expect(page.getByRole('main')).toHaveCount(1)
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toHaveAttribute(
    'data-total-loaded',
    importsProductFixture.loadedImports,
  )
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
  expect(await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__)).toBeUndefined()
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
  await page.getByRole('button', { name: /Go to Settings/ }).click()
  await expect(page).toHaveURL(/\/settings$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('withholds Imports until bootstrap admission succeeds', async ({ page }) => {
  const problems = watchBrowser(page)
  let importRequests = 0
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ status: 503, body: 'temporarily unavailable' }),
  )
  await page.route('**/api/imports/**', (route) => {
    importRequests += 1
    return route.abort()
  })
  await page.goto(importsProductFixture.path)

  await expect(
    page.getByRole('heading', {
      name: 'Imports are unavailable until bootstrap admission succeeds',
    }),
  ).toBeVisible()
  await expect(page.getByText('Transport unavailable')).toBeVisible()
  await expect(page.locator('.imports-shell-product')).toHaveCount(0)
  expect(importRequests).toBe(0)
  expect(problems.pageErrors).toEqual([])
})

test('mounts Imports after the daemon contract recovers', async ({ page }) => {
  const problems = watchBrowser(page)
  const admission = await useBootstrapRecoveringAfterOneOutage(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  await expect(page.getByText('Transport unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry contract' }).click()

  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(admission.attempts).toBe(2)
  expect(problems.pageErrors).toEqual([])
})

test('serves exact source-session searches through the deterministic adapter', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  const rows = page.getByRole('rowgroup', { name: 'Imported conversation rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', importsProductFixture.loadedImports)
  await page
    .getByRole('textbox', { name: 'Filter imports by exact source session evidence' })
    .fill('source-session-0')
  await page.getByRole('checkbox', { name: 'Use exact source session filter' }).check()

  await expect(rows).toHaveAttribute('data-total-loaded', '1')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('lights up the imports command family only on the Imports surface', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette.getByRole('button', { name: /Select next imported frontier/ })).toBeVisible()
  await palette.getByRole('button', { name: /Go to Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)

  await page.keyboard.press(`${modifier}+K`)
  await expect(palette.getByRole('button', { name: /Select next imported frontier/ })).toHaveCount(
    0,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends Imports hotkeys while the palette owns keyboard scope', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  // An open palette hides the rest of the page from the accessibility tree, so address the list
  // structurally rather than by role.
  const entries = page.locator('[aria-label="Imported source entries"]')
  await entries.focus()
  const initialSelection = await entries.getAttribute('aria-activedescendant')
  expect(initialSelection).not.toBeNull()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('j')
  await expect(entries).toHaveAttribute('aria-activedescendant', initialSelection ?? '')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('advances the imported frontier exactly once per product hotkey', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await entries.focus()
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    '1',
  )
  await page.keyboard.press('j')
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    '2',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies product presentation controls to the mounted Imports surface', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()

  await page.getByRole('button', { name: 'Use comfortable density' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('scrolls short Imports workbenches instead of clipping the inspector', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.setViewportSize({ width: 1200, height: 560 })
  await page.goto(importsProductFixture.path)

  const main = page.locator('.product-main-imports')
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(await main.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true)
  await main.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(page.getByRole('heading', { name: 'Import inspector' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('switches Imports layout before the product pane clips', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.setViewportSize({ width: 920, height: 844 })
  await page.goto(importsProductFixture.path)

  const workspace = page.locator('.imports-workspace-product')
  await expect(workspace).toBeVisible()
  expect(await workspace.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
    true,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('stacks Imports from the available product pane width', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.addInitScript(() => {
    localStorage.setItem(
      'signalbox.web.preferences.v1',
      JSON.stringify({
        layout: 'workbench',
        density: 'compact',
        detail: 'condensed',
        theme: 'dark',
        paneSizes: { navigation: 360, inspector: 480 },
      }),
    )
  })
  await page.setViewportSize({ width: 1280, height: 844 })
  await page.goto(importsProductFixture.path)

  const workspace = page.locator('.imports-workspace-product')
  const inspectorBody = page.locator('.import-inspector-body')
  await expect(workspace).toBeVisible()
  await expect(inspectorBody).toHaveCSS('grid-template-columns', /^(?!.* ).+$/)
  expect(await workspace.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
    true,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('locks product navigation while an ambiguous continuation command is retained', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.route('**/api/imports/*/continuations', (route) =>
    route.fulfill({
      status: 503,
      json: {
        error: {
          kind: 'application',
          code: 'continuation_commit_ambiguous',
          message: 'The commit outcome is ambiguous.',
        },
      },
    }),
  )
  await page.goto(importsProductFixture.path)

  await page
    .getByRole('textbox', { name: 'Initial model selection UUID' })
    .fill('00000000-0000-7000-8000-000000000777')
  await page.getByRole('button', { name: 'Resume' }).click()
  await expect(page.getByRole('button', { name: 'Retry exact command' })).toBeVisible()

  const settingsLink = page.getByRole('link', { name: /Settings/ })
  await expect(settingsLink).toHaveAttribute('aria-disabled', 'true')
  await settingsLink.click({ force: true })
  await expect(page).toHaveURL(/\/imports$/)
  await page.keyboard.press('g')
  await page.keyboard.press(',')
  await expect(page).toHaveURL(/\/imports$/)
  await expect(page.getByRole('button', { name: 'Retry exact command' })).toBeVisible()
  const expectedResourceError =
    'Failed to load resource: the server responded with a status of 503 (Service Unavailable)'
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.every((error) => error === expectedResourceError)).toBe(true)
})

test('runs advertised product navigation sequences', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
  await page.keyboard.press('g')
  await page.keyboard.press(',')
  await expect(page).toHaveURL(/\/settings$/)
  await expect(page).toHaveTitle('Settings · Signalbox')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not run product navigation sequences while a modal owns focus', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await palette.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(palette).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not run product view hotkeys while a modal owns focus', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  const presentationBefore = await page.evaluate(() => ({
    theme: document.documentElement.dataset.theme,
    density: document.documentElement.dataset.density,
  }))

  await page.getByRole('button', { name: 'Open command palette' }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await palette.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('Shift+T')
  await page.keyboard.press('Shift+D')
  await page.keyboard.press('Shift+W')
  expect(
    await page.evaluate(() => ({
      theme: document.documentElement.dataset.theme,
      density: document.documentElement.dataset.density,
    })),
  ).toEqual(presentationBefore)
  await expect(palette).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not run product view hotkeys while the artifact sheet owns focus', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')
  const presentationBefore = await page.evaluate(() => ({
    theme: document.documentElement.dataset.theme,
    density: document.documentElement.dataset.density,
  }))

  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  const sheet = page.getByRole('dialog', { name: 'Artifact inspector' })
  await expect(sheet).toBeVisible()
  await sheet.getByRole('button', { name: 'Close artifact inspector' }).focus()
  await page.keyboard.press('Shift+T')
  await page.keyboard.press('Shift+D')
  await page.keyboard.press('Shift+W')

  expect(
    await page.evaluate(() => ({
      theme: document.documentElement.dataset.theme,
      density: document.documentElement.dataset.density,
    })),
  ).toEqual(presentationBefore)
  await expect(sheet).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('unwinds the phone navigation sheet with Escape', async ({ page }) => {
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

test('closes the phone navigation sheet before entering Scenario studio', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(navigation).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('retries a transient bootstrap failure without reloading', async ({ page }) => {
  const problems = watchBrowser(page)
  const scenario = await useRecoveringBootstrap(page)
  await page.goto('/attention')

  await expect(page.getByText('Bootstrap unavailable')).toBeVisible()
  scenario.recover()
  await page.getByRole('button', { name: 'Retry bootstrap' }).click()
  await expect(
    page.getByText(`${bootstrapFixture.contract.name} · ${bootstrapFixture.contract.version}`),
  ).toBeVisible()
  await expect(page.getByRole('status')).toBeFocused()
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every((message) =>
      message.includes('Failed to load resource: the server responded with a status of 503'),
    ),
  ).toBe(true)
})

test('distinguishes a rejected bootstrap contract from transport failure', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  await expect(page.getByText('Contract rejected')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry bootstrap' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

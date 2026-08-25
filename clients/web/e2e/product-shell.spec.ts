import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'

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
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => {
    attempts += 1
    if (attempts === 1) {
      return route.fulfill({ status: 503, body: 'temporarily unavailable' })
    }
    return route.fulfill({ json: bootstrapFixture })
  })
  await page.goto('/sessions')

  await expect(page.getByText('Transport unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry contract' }).click()

  await expect(page.getByText('Timeline reads available')).toBeVisible()
  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
  expect(attempts).toBe(2)
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

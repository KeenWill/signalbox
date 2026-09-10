import { createHash } from 'node:crypto'
import { ScenarioImportApi } from '../src/imports/scenario'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'
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

const emptyAttentionFixture = {
  continuation_after_session_id: null,
  cursor: '0',
  summaries: [],
} as const
const emptySessionCatalogFixture = {
  continuation: null,
  cursor: '0',
  sort: 'last_activity_descending',
  summaries: [],
  total: '0',
} as const

const useDeterministicAttention = async (page: Page) => {
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: emptyAttentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => route.fulfill({ json: emptyAttentionFixture }))
}

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({ json: emptySessionCatalogFixture }),
  )
  await useDeterministicAttention(page)
}

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
    if (pathname.endsWith('/timeline-detail')) {
      const query = new URL(route.request().url()).searchParams
      const items = ['41', '42', '43']
        .filter(
          (address) =>
            BigInt(address) >= BigInt(query.get('first') ?? '41') &&
            BigInt(address) <= BigInt(query.get('through') ?? '43'),
        )
        .map((address) => {
          const kind =
            timelineKind(requestedSessionId, address) ??
            (address === '41'
              ? 'input_accepted'
              : address === '42'
                ? 'turn_activated'
                : 'turn_completed')
          const body =
            kind === 'input_accepted'
              ? {
                  type: 'user_input',
                  turn_id: requestedSessionId,
                  text: { text: '', offset_bytes: '0', total_bytes: '0', continuation: null },
                  attachments: [],
                }
              : {
                  type: 'turn_lifecycle',
                  turn_id: requestedSessionId,
                  lifecycle: kind === 'turn_activated' ? 'activated' : 'terminalized',
                  cause_code: kind.slice('turn_'.length),
                }
          return { address: { event_sequence: address }, kind, body, projected_body_bytes: 128 }
        })
      return route.fulfill({
        json: {
          session_id: requestedSessionId,
          items,
          projected_body_bytes: items.length * 128,
          continuation: null,
        },
      })
    }
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
        repository_watch: null,
        workspace_root_kind: null,
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

test('applies saved visual preferences before the first rendered frame', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem(
      'signalbox.web.preferences.v1',
      JSON.stringify({
        layout: 'workbench',
        density: 'comfortable',
        detail: 'condensed',
        theme: 'light',
        paneSizes: { navigation: 218, inspector: 252 },
        lastLogicalPositions: {},
      }),
    )
    const observed: string[] = []
    Object.defineProperty(window, '__visualPreferenceMutations', { value: observed })
    document.addEventListener('DOMContentLoaded', () => {
      requestAnimationFrame(() => {
        observed.push(
          `${document.documentElement.dataset.theme}:${document.documentElement.dataset.density}`,
        )
      })
    })
  })
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window as typeof window & { __visualPreferenceMutations: string[] })
            .__visualPreferenceMutations[0],
      ),
    )
    .toBe('light:comfortable')
})

test('opens the product at Attention after bootstrap admission', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page).toHaveTitle('Attention · Signalbox')
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
  await expect(page.locator('.product-connection')).toHaveCount(0)
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

test('focuses Scenario studio after cross-route navigation', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Scenario studio/ }).click()

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page.locator('main.workspace')).toBeFocused()
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
  await page.goto('/sessions?workspace=true')

  await expect(page.getByText('Session timeline unavailable')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open', exact: true })).toBeDisabled()
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
  await page.goto('/sessions?workspace=true')

  await expect(page.getByText('Session timeline unavailable')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open', exact: true })).toBeDisabled()
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
  await page.goto('/sessions?workspace=true')

  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  scenario.recover()
  await page.getByRole('button', { name: 'Retry connection' }).click()

  await expect(page.getByText('Session ID required')).toBeVisible()
  await expect(page.locator('.product-connection')).toHaveCount(0)
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
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus after closing the command palette', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const trigger = page.getByRole('button', { name: 'Open command palette' })
  await trigger.click()
  await page.keyboard.press('Escape')
  await expect(trigger).toBeFocused()
})

test('returns a hotkey-opened palette to its invoking control', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const sessions = page.getByRole('link', { name: /Sessions/ })
  await sessions.focus()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.keyboard.press('Escape')

  await expect(sessions).toBeFocused()
})

test('renders a truthful search bootstrap failure', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ status: 503 }))
  await page.goto('/search')

  await expect(page.getByRole('heading', { name: 'Search unavailable' })).toBeVisible()
  await expect(page.getByText('Loading search…')).toHaveCount(0)
})

test('does not offer the palette opener inside the open palette', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })

  await expect(palette.getByRole('button', { name: /Open command palette/ })).toHaveCount(0)
})

test('sets route-aware product document titles', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/search')
  await expect(page).toHaveTitle('Search · Signalbox')

  await page.getByRole('link', { name: /Attention/ }).click()
  await expect(page).toHaveTitle('Attention · Signalbox')
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

test('uses the main pane for sessions and expands it in Focus', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`/sessions?workspace=true&session=${sessionWorkspaceFixture.id}`)
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByRole('complementary', { name: 'Inspector' })).toHaveCount(0)
  const main = page.getByRole('main')
  const workbench = await main.boundingBox()
  expect(workbench!.width).toBeGreaterThan(1200)
  await page.getByRole('button', { name: 'Switch to focus layout' }).click()
  await expect.poll(async () => (await main.boundingBox())?.width).toBe(1440)
  await expect(page.getByRole('navigation', { name: 'Product navigation' })).toBeHidden()
  await page.getByRole('button', { name: 'Switch to workbench layout' }).click()
  await expect.poll(async () => (await main.boundingBox())?.width).toBe(workbench!.width)
})

test('opens and inspects a bounded production session without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
  await expect(page.getByText(sessionWorkspaceFixture.itemCount, { exact: true })).toBeVisible()
  await expect(page.getByRole('form', { name: 'Message composer' })).toBeVisible()
  const timeline = page.getByRole('grid', { name: 'Session timeline' })
  await expect(page.getByRole('row', { name: /41 Message accepted/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  await expect(timeline).toHaveAttribute('aria-activedescendant', 'session-timeline-option-41')
  const latest = page.getByRole('button', { name: /Latest/ })
  const reconnect = page.getByRole('button', { name: 'Reconnect live updates' })
  await expect(reconnect).toBeVisible()
  await latest.focus()
  await page.keyboard.press('Tab')
  await expect(reconnect).toBeFocused()
  await page.keyboard.press('Tab')
  await expect(page.getByRole('checkbox', { name: 'Events', exact: true })).toBeFocused()
  await page.keyboard.press('Tab')
  await expect(timeline).toBeFocused()
  await latest.focus()
  await page.keyboard.press('j')
  await expect(timeline).toBeFocused()
  await expect(page.getByRole('row', { name: /42 Turn started/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )

  const completed = page.getByRole('row', { name: /43 Turn completed/ })
  await completed.click()
  await expect(timeline).toBeFocused()
  await expect(completed).toHaveAttribute('aria-controls', 'session-timeline-detail-43')
  await expect(completed).toHaveAttribute('aria-describedby', 'session-timeline-disclosure-43')
  await expect(page.locator('#session-timeline-disclosure-43')).toHaveText('Expanded')
  await expect(page.locator('#session-timeline-detail-43')).toBeVisible()
  await expect(
    page.getByRole('article', { name: 'Turn completed detail' }).getByText('Finished'),
  ).toBeVisible()
  await expect(page.getByRole('complementary', { name: 'Inspector', exact: true })).toHaveCount(0)

  const accepted = page.getByRole('row', { name: /41 Message accepted/ })
  await accepted.getByText('Message accepted', { exact: true }).click()
  await expect(page.locator('#session-timeline-detail-41')).toBeVisible()
  await expect(page.getByRole('region', { name: 'Accepted input', exact: true })).toBeVisible()
  await accepted.getByText('Message accepted', { exact: true }).click()
  await expect(page.locator('#session-timeline-detail-41')).toBeHidden()

  const first = page.getByRole('button', { name: /First/ })
  await first.click()
  await expect(first).toBeFocused()
  await expect(page.getByRole('row', { name: /41 Message accepted/ })).toHaveAttribute(
    'aria-selected',
    'true',
  )
  await latest.click()
  await expect(latest).toBeFocused()
  await expect(completed).toHaveAttribute('aria-selected', 'true')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gives Full and Condensed distinct Session presentations', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.locator('.session-item-summary small').first()).toBeHidden()

  await page.getByRole('link', { name: /Settings/ }).click()
  await page.getByRole('radio', { name: 'Full' }).check()
  await page.getByRole('link', { name: /Sessions/ }).click()
  await page.getByRole('button', { name: 'Open by ID' }).click()
  const reopenedSessionId = page.getByRole('textbox', { name: 'Session ID' })
  await reopenedSessionId.fill(sessionWorkspaceFixture.id)
  await reopenedSessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()

  await expect(page.locator('.session-item-summary small').first()).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps palette selection commands focused on the Session timeline', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  const timeline = page.getByRole('grid', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Select next timeline item/ }).click()

  await expect(timeline).toBeFocused()
  await expect(page.getByRole('row', { name: /42 Turn started/ })).toHaveAttribute(
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
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await page.getByRole('row', { name: /43 Turn completed/ }).click()

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
    problems.consoleErrors.every(
      (message) =>
        message.includes('Failed to load resource: the server responded with a status of 503') ||
        message.startsWith('Session load failed'),
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
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await page.getByRole('row', { name: /43 Turn completed/ }).click()

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
    problems.consoleErrors.every(
      (message) =>
        message.includes('Failed to load resource: the server responded with a status of 503') ||
        message.startsWith('Session load failed'),
    ),
  ).toBe(true)
})

test('clears cached Session projections after a refetch error', async ({ page }) => {
  const problems = watchBrowser(page)
  let failTimeline = false
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page, () => failTimeline)
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeVisible()

  failTimeline = true
  await page.getByRole('button', { name: /Latest/ }).click()

  await expect(page.getByRole('alert')).toContainText("Session couldn't be loaded.")
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toHaveCount(0)
  await expect(
    page
      .getByRole('complementary', { name: 'Inspector', exact: true })
      .getByText(sessionWorkspaceFixture.id),
  ).toHaveCount(0)
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every(
      (message) =>
        message.includes('Failed to load resource: the server responded with a status of 503') ||
        message.startsWith('Session load failed'),
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
  await page.goto('/sessions?workspace=true')

  const sessionId = page.getByRole('textbox', { name: 'Session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('row', { name: /43 Turn completed/ })).toBeVisible()

  contradictRetainedEvent = true
  await page.getByRole('button', { name: /Latest/ }).click()

  await expect(page.getByRole('alert')).toContainText("Session couldn't be loaded.")
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
  await expect(navigation).toBeHidden()
  await expect(page).toHaveURL(/\/sessions$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('runs product navigation sequences but leaves Mod+K to an editing field', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.focus()
  await search.evaluate((element) => {
    element.addEventListener('keydown', (event) => {
      queueMicrotask(() => {
        element.dataset.modKDefaultPrevented = String(event.defaultPrevented)
      })
    })
  })
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  await expect(search).toHaveAttribute('data-mod-k-default-prevented', 'false')

  // Navigation sequences remain ordinary text while the search field owns focus.
  await expect(search).toBeFocused()
  await page.keyboard.press('g')
  await page.keyboard.press('a')
  await expect(page).toHaveURL(/\/search$/)

  // Away from the editing context the same sequence runs.
  await page.getByRole('button', { name: 'Open command palette' }).focus()
  await page.keyboard.press('g')
  await page.keyboard.press('a')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suppresses product navigation sequences while an overlay owns input', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
})

test('leaves unavailable scenario sequences inert on product routes', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const paletteButton = page.getByRole('button', { name: 'Open command palette' })
  await paletteButton.focus()
  await page.keyboard.press('g')
  await page.keyboard.press('g')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(paletteButton).toBeFocused()
})

test('suppresses ordinary view hotkeys while an overlay owns input', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.keyboard.press('Shift+D')
  await page.keyboard.press('Shift+T')
  await page.keyboard.press('Shift+W')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'compact')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
  await expect(page.locator('.product-shell')).toHaveClass(/layout-workbench/)
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
})

test('restores focus before focus layout hides product navigation', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: 'Attention' }).focus()
  await page.keyboard.press('Shift+W')

  await expect(page.locator('.product-shell')).toHaveClass(/layout-focus/)
  await expect(page.getByRole('main')).toBeFocused()
})

test('changes visible product spacing with the density control', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/search')

  const surface = page.locator('.surface-body')
  const compactPadding = await surface.evaluate((element) => getComputedStyle(element).paddingTop)
  await page.getByRole('main').focus()
  await page.keyboard.press('Shift+D')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  const comfortablePadding = await surface.evaluate(
    (element) => getComputedStyle(element).paddingTop,
  )
  expect(comfortablePadding).not.toBe(compactPadding)
})

test('retries an initial bootstrap failure', async ({ page }) => {
  const problems = watchBrowser(page)
  const expectedFailureMessage =
    'Failed to load resource: the server responded with a status of 503 (Service Unavailable)'
  await useBootstrapRecoveringAfterOneOutage(page)
  // Attention reads start as soon as the retried bootstrap is admitted; serving them
  // deterministically keeps the staged outage the only console error this scenario sees.
  await useDeterministicAttention(page)
  await page.goto('/attention')

  // A refused admission answers with a status, so `readBootstrap` raises a plain error and the
  // shell classifies it as an unavailable bootstrap rather than an unreachable transport.
  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await expect(page.locator('.product-connection')).toHaveCount(0)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.filter((message) => message !== expectedFailureMessage)).toEqual([])
})

test('distinguishes an incompatible bootstrap contract from an outage', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  // A schema-invalid payload decodes into a contract error, which the shell reports as a rejected
  // contract. Addressed by text because a deferred surface also publishes a `status` region.
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
})

test('shows transcript-detail commands only on Settings among product routes', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('button', { name: /Show full transcript detail/ })).toHaveCount(0)
  await page.keyboard.press('Escape')

  await page.goto('/settings')
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('button', { name: /Show full transcript detail/ })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps Settings within the pane when a vertical scrollbar reduces content width', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 840, height: 480 })
  await page.goto('/settings')

  const navigationWidth = page
    .getByRole('group', { name: 'Pane widths' })
    .getByRole('slider')
    .nth(0)
  await navigationWidth.fill('360')

  const settingsSurface = page.locator('.settings-surface')
  expect(
    await settingsSurface.evaluate((element) => element.scrollWidth <= element.clientWidth),
  ).toBe(true)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies saved pane widths to the scenario workspace', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/settings')

  const paneSliders = page.getByRole('group', { name: 'Pane widths' }).getByRole('slider')
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

test('fits Settings inside a narrow primary pane with maximum saved side panes', async ({
  page,
}, testInfo) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 780, height: 720 })
  await page.goto('/settings')
  const sliders = page.getByRole('group', { name: 'Pane widths' }).getByRole('slider')
  await sliders.nth(0).fill('360')
  await sliders.nth(1).fill('480')

  const settings = page.locator('.settings-surface')
  await expect(settings).toBeVisible()
  expect(await settings.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
    true,
  )
  const layoutBox = await page.getByRole('group', { name: 'Layout', exact: true }).boundingBox()
  const densityBox = await page.getByRole('group', { name: 'Density', exact: true }).boundingBox()
  expect(densityBox?.y).toBeGreaterThan((layoutBox?.y ?? 0) + (layoutBox?.height ?? 0))
  await page.screenshot({ path: testInfo.outputPath('settings-pane.png') })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('compacts the scenario toolbar at its pane width', async ({ page }, testInfo) => {
  const problems = watchBrowser(page)
  await page.setViewportSize({ width: 780, height: 720 })
  await page.goto('/settings')
  await page.getByRole('group', { name: 'Pane widths' }).getByRole('slider').nth(0).fill('360')
  await page.getByRole('link', { name: /Scenario studio/ }).click()

  const toolbar = page.getByRole('toolbar', { name: 'Workspace controls' })
  await expect(toolbar).toBeVisible()
  await expect(page.getByRole('group', { name: 'Transcript detail' })).toBeHidden()
  expect(await toolbar.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  const toolbarBox = await toolbar.boundingBox()
  expect((toolbarBox?.x ?? 0) + (toolbarBox?.width ?? 0)).toBeLessThanOrEqual(780)
  await page.screenshot({ path: testInfo.outputPath('scenario-pane.png') })
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect(page.getByRole('group', { name: 'Transcript detail' })).toBeVisible()
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
  await expect(page.getByRole('group', { name: 'Theme', exact: true })).toBeVisible()
  await expect(page.getByText('Daemon unreachable')).toHaveCount(0)
  expect(bootstrapRequests).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('offers Scenario Studio through the product command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Go to Scenario studio/ }).click()

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not start Attention reads when bootstrap validation fails', async ({ page }) => {
  let attentionRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.route('**/api/attention**', (route) => {
    attentionRequests += 1
    return route.abort()
  })

  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention unavailable' })).toBeVisible()
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
  expect(attentionRequests).toBe(0)
})

test('does not start Attention reads for incompatible bootstrap values', async ({ page }) => {
  let attentionRequests = 0
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_ndjson_item_bytes: 32_768 },
      },
    }),
  )
  await page.route('**/api/attention**', (route) => {
    attentionRequests += 1
    return route.abort()
  })

  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention unavailable' })).toBeVisible()
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
  expect(attentionRequests).toBe(0)
})

test('retries a transient Attention bootstrap failure in place', async ({ page }) => {
  const admission = await useBootstrapRecoveringAfterOneOutage(page)
  await useDeterministicAttention(page)
  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention unavailable' })).toBeVisible()
  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  await expect(page.getByRole('button', { name: /^Retry/ })).toHaveCount(1)
  await page.getByRole('button', { name: 'Retry connection', exact: true }).click()

  await expect(page.locator('.product-connection')).toHaveCount(0)
  await expect(page.getByRole('heading', { name: '0 sessions' })).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
  expect(admission.attempts).toBe(2)
})

test('gives iconless Attention contract errors the full empty-state width', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  const message = page.getByRole('heading', { name: 'Attention unavailable' }).locator('..')
  await expect(message).toHaveCSS('grid-column-start', '1')
  await expect(message).toHaveCSS('grid-column-end', '-1')
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
      name: 'Imports unavailable',
    }),
  ).toBeVisible()
  // A refused admission answers with a status, so the shell classifies it as an unavailable
  // bootstrap rather than as an unreachable transport or a rejected contract.
  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  await expect(page.locator('.imports-shell-product')).toHaveCount(0)
  expect(importRequests).toBe(0)
  expect(problems.pageErrors).toEqual([])
})

test('shares expired Imports admission failure with the shell retry state', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.clock.install()
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.clock.fastForward(30_001)
  await page.getByRole('textbox', { name: 'Source session' }).fill('expired-admission')
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry connection' })).toBeVisible()
  await useDeterministicBootstrap(page)
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('withholds Search focus after cached bootstrap refetch failure and restores it on retry', async ({
  page,
}) => {
  await page.clock.install()
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.clock.fastForward(30_001)
  await page.getByRole('textbox', { name: 'Source session', exact: true }).fill('expired-admission')
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
  await page.getByRole('link', { name: 'Search', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Search unavailable', exact: true })).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await expect(page.getByRole('button', { name: /Focus search/ })).toHaveCount(0)
  await page.keyboard.press('Escape')
  await page.evaluate(() => {
    let shortcutEvent: KeyboardEvent | undefined
    document.addEventListener(
      'keydown',
      (event) => {
        if (event.shiftKey && event.key.toLowerCase() === 'f') {
          shortcutEvent = event
        }
      },
      true,
    )
    document.addEventListener('keyup', (event) => {
      if (event.key.toLowerCase() === 'f' && shortcutEvent) {
        document.body.dataset.searchFocusDefaultPrevented = String(shortcutEvent.defaultPrevented)
      }
    })
  })
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+Shift+f`)
  await expect(page.locator('body')).toHaveAttribute('data-search-focus-default-prevented', 'false')

  await useDeterministicBootstrap(page)
  await page.getByRole('button', { name: 'Retry connection', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeVisible()
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Focus search/ }).click()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
  await page.keyboard.press('Escape')
  await page.keyboard.press(`${modifier}+Shift+f`)
  await expect(page.locator('body')).toHaveAttribute('data-search-focus-default-prevented', 'true')
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
})

test('mounts Imports after the daemon contract recovers', async ({ page }) => {
  const problems = watchBrowser(page)
  const admission = await useBootstrapRecoveringAfterOneOutage(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry connection' }).click()

  await expect(page.locator('.product-connection')).toHaveCount(0)
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
  await page.getByRole('textbox', { name: 'Source session' }).fill('source-session-0')
  await page.getByRole('checkbox', { name: 'Filter by source' }).check()

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
  await expect(palette.getByRole('button', { name: /Next entry/ })).toBeVisible()
  await palette.getByRole('button', { name: /Go to Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)

  await page.keyboard.press(`${modifier}+K`)
  await expect(palette.getByRole('button', { name: /Next entry/ })).toHaveCount(0)
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

  await page.getByRole('main').focus()
  await page.keyboard.press('Shift+D')
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
        lastLogicalPositions: {},
      }),
    )
  })
  await page.setViewportSize({ width: 1280, height: 844 })
  await page.goto(importsProductFixture.path)

  await page.getByRole('button', { name: 'Open artifact inspector', exact: true }).click()
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

  await page.getByRole('textbox', { name: 'Model ID' }).fill('00000000-0000-7000-8000-000000000777')
  await page.getByRole('button', { name: 'Resume' }).click()
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()

  const settingsLink = page.getByRole('link', { name: /Settings/ })
  await expect(settingsLink).toHaveAttribute('aria-disabled', 'true')
  await settingsLink.click({ force: true })
  await expect(page).toHaveURL(/\/imports$/)
  await page.keyboard.press('g')
  await page.keyboard.press(',')
  await expect(page).toHaveURL(/\/imports$/)
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
  const expectedResourceError =
    'Failed to load resource: the server responded with a status of 503 (Service Unavailable)'
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.every((error) => error === expectedResourceError)).toBe(true)
})

test('retains exact retry after a lost acknowledgement and corrupt continuation receipt', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  const requests: string[] = []
  await page.route('**/api/imports/*/continuations', (route) => {
    requests.push(route.request().postData() ?? '')
    if (requests.length === 1) return route.abort('failed')
    if (requests.length > 2) return route.fallback()
    return route.fulfill({
      status: 500,
      json: {
        error: { kind: 'application', code: 'continuation_corrupt', message: 'Corrupt evidence.' },
      },
    })
  })
  await page.goto(importsProductFixture.path)
  await page.getByRole('textbox', { name: 'Model ID' }).fill('00000000-0000-7000-8000-000000000777')
  await page.getByRole('button', { name: 'Resume' }).click()
  await expect(page.getByRole('alert')).toContainText('Outcome unknown.')
  const corruptReceipt = page.waitForResponse((response) => response.status() === 500)
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await corruptReceipt
  await expect(page.getByRole('alert')).toContainText('Outcome unknown.')
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
  await expect(page.getByRole('link', { name: /Settings/ })).toHaveAttribute(
    'aria-disabled',
    'true',
  )
  await page.getByRole('link', { name: /Settings/ }).click({ force: true })
  await expect(page).toHaveURL(/\/imports$/)
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.getByText('Session created:', { exact: false })).toBeVisible()
  expect(requests).toHaveLength(3)
  expect(requests[1]).toBe(requests[0])
  expect(requests[2]).toBe(requests[0])
  await expect(page.getByRole('alert')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0)
  expect(problems.pageErrors).toEqual([])
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
  // Attention reads start as soon as the retried bootstrap is admitted; serving them
  // deterministically keeps the staged bootstrap outage the only console error this scenario sees.
  await useDeterministicAttention(page)
  await page.goto('/attention')

  await expect(page.getByText('Daemon unavailable')).toBeVisible()
  scenario.recover()
  await page.getByRole('button', { name: 'Retry connection' }).click()
  await expect(page.locator('.product-connection')).toHaveCount(0)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems.pageErrors).toEqual([])
  expect(
    problems.consoleErrors.every(
      (message) =>
        message.includes('Failed to load resource: the server responded with a status of 503') ||
        message.startsWith('Session load failed'),
    ),
  ).toBe(true)
})

test('distinguishes a rejected bootstrap contract from transport failure', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry connection' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores keyboard help focus to the opener through hotkey and palette entry', async ({
  page,
}) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  const sessions = page.getByRole('link', { name: /Sessions/ })
  await sessions.focus()
  await page.keyboard.press('Shift+/')
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(sessions).toBeFocused()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.getByRole('button', { name: 'Close keyboard help' }).click()
  await expect(sessions).toBeFocused()
})

test('retains window control focus while loading and after the new window arrives', async ({
  page,
}) => {
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions?workspace=true')
  await page.getByRole('textbox', { name: 'Session ID' }).fill(sessionWorkspaceFixture.id)
  await page.getByRole('button', { name: 'Open', exact: true }).click()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
  let releaseWindow = () => {}
  const windowReady = new Promise<void>((resolve) => {
    releaseWindow = resolve
  })
  let requested = false
  await page.route('**/api/sessions/**/timeline?**', async (route) => {
    requested = true
    await windowReady
    await route.fallback()
  })
  const first = page.getByRole('button', { name: /First/ })
  await first.focus()
  await page.keyboard.press('Enter')
  await expect.poll(() => requested).toBe(true)
  await expect(first).toBeFocused()
  releaseWindow()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
  await expect(first).toBeFocused()
  const latest = page.getByRole('button', { name: /Latest/ })
  await latest.click()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
  await expect(latest).toBeFocused()
})

test('trims the session identity before native form validation', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions?workspace=true')
  const input = page.getByRole('textbox', { name: 'Session ID' })
  await input.fill(`  ${sessionWorkspaceFixture.id}  `)
  await expect(input).toHaveValue(sessionWorkspaceFixture.id)
  await input.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
})

test('starts the settled exact import filter without the previous page cursor', async ({
  page,
}) => {
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  const firstPage = await new ScenarioImportApi().list({ limit: 100 })
  const requests: { source: string; after: string | null }[] = []
  await page.route('**/api/imports/searches?**', async (route) => {
    const url = new URL(route.request().url())
    const source = route.request().postData() ?? ''
    const after = url.searchParams.get('after')
    requests.push({ source, after })
    const digest = createHash('sha256').update(source).digest('hex')
    const hasPage = source === 'source-session-0' && after === null
    await route.fulfill({
      json: {
        items: hasPage
          ? firstPage.items.map((item) => ({
              ...item,
              source_session_id: { leading_text: source, completeness: 'complete' },
              source_session_id_sha256: digest,
            }))
          : [],
        next_cursor: hasPage ? firstPage.next_cursor : undefined,
        search_correlation: url.searchParams.get('search_correlation'),
        exact_source_session_id_sha256: digest,
      },
    })
  })
  await page.clock.install()
  await page.goto('/imports')
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  await page.clock.pauseAt(new Date(Date.now() + 1_000))
  const input = page.getByRole('textbox', {
    name: 'Source session',
  })
  await input.fill('source-session-0')
  await page.clock.runFor(200)
  await page.getByRole('checkbox', { name: 'Filter by source' }).check()
  await expect.poll(() => requests.at(-1)?.source).toBe('source-session-0')
  await expect(page.getByRole('button', { name: 'Next', exact: true })).toBeEnabled()
  await input.fill('source-session-1')
  await page.getByRole('button', { name: 'Next', exact: true }).click()
  await expect
    .poll(() =>
      requests.some((request) => request.source === 'source-session-0' && request.after !== null),
    )
    .toBe(true)
  await page.clock.runFor(200)
  await expect.poll(() => requests.at(-1)).toEqual({ source: 'source-session-1', after: null })
})

for (const surface of ['attention', 'sessions'] as const) {
  test(`keeps connection recovery controls inside a phone viewport on ${surface}`, async ({
    page,
  }, testInfo) => {
    await page.setViewportSize({ width: 390, height: 844 })
    await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invalid: true } }))
    await page.goto(`/${surface}`)
    const retry = page.getByRole('button', { name: 'Retry connection', exact: true })
    const palette = page.getByRole('button', { name: 'Open command palette', exact: true })
    await expect(retry).toBeVisible()
    for (const control of [retry, palette]) {
      const box = await control.boundingBox()
      expect(box).not.toBeNull()
      expect(box?.x).toBeGreaterThanOrEqual(0)
      expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(390)
    }
    expect(await page.locator('.product-shell').evaluate((element) => element.scrollWidth)).toBe(
      390,
    )
    await page.screenshot({ path: testInfo.outputPath('phone-connection-error.png') })
    await expect(page.getByRole('button', { name: /^Retry/ })).toHaveCount(1)
  })
}

for (const entry of ['button', 'palette'] as const) {
  test(`hides empty import controls and retries failed discovery from the ${entry}`, async ({
    page,
  }) => {
    await useDeterministicBootstrap(page)
    await useDeterministicImportApi(page)
    let mode: 'empty' | 'failed' | 'ready' = 'empty'
    await page.route('**/api/imports/**', async (route) => {
      if (
        new URL(route.request().url()).pathname.replace(/\/$/, '') !== '/api/imports' ||
        mode === 'ready'
      )
        return route.fallback()
      return mode === 'empty'
        ? route.fulfill({ json: { items: [] } })
        : route.fulfill({ status: 503, body: 'unavailable' })
    })
    await page.goto('/imports')
    await expect(page.getByRole('table', { name: 'Imported conversations' })).toBeVisible()
    await expect(page.locator('.import-inspector')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0)
    mode = 'failed'
    await page.reload()
    await expect(page.getByRole('alert')).toContainText('Imports unavailable')
    await expect(page.locator('.import-inspector')).toHaveCount(0)
    mode = 'ready'
    const opener = page.getByRole('button', { name: 'Open command palette', exact: true })
    if (entry === 'palette') await opener.click()
    const retry =
      entry === 'button'
        ? page.getByRole('button', { name: 'Retry imports', exact: true })
        : page
            .getByRole('dialog', { name: 'Command palette' })
            .getByRole('button', { name: /Retry imports/ })
    await retry.focus()
    await retry.press('Enter')
    await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
    await expect(page.locator('.import-inspector')).toBeVisible()
    await expect(
      entry === 'button' ? page.getByRole('region', { name: 'Imports', exact: true }) : opener,
    ).toBeFocused()
    await opener.click()
    await expect(
      page
        .getByRole('dialog', { name: 'Command palette' })
        .getByRole('button', { name: /Retry imports/ }),
    ).toHaveCount(0)
  })
}

for (const outcome of ['success', 'rejection'] as const) {
  test(`keeps a restored import continuation ${outcome} visible while discovery fails`, async ({
    page,
  }) => {
    const problems = watchBrowser(page)
    await useDeterministicBootstrap(page)
    await useDeterministicImportApi(page)
    const requests: string[] = []
    await page.route('**/api/imports/*/continuations', (route) => {
      requests.push(route.request().postData() ?? '')
      if (requests.length === 1) return route.abort('failed')
      return outcome === 'success'
        ? route.fallback()
        : route.fulfill({
            status: 400,
            json: {
              error: {
                kind: 'application',
                code: 'invalid_import_request',
                message: 'Request rejected.',
              },
            },
          })
    })
    await page.goto(importsProductFixture.path)
    await page
      .getByRole('textbox', { name: 'Model ID' })
      .fill('00000000-0000-7000-8000-000000000777')
    await page.getByRole('button', { name: 'Resume', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
    await page.route('**/api/imports/**', (route) =>
      new URL(route.request().url()).pathname.replace(/\/$/, '') === '/api/imports'
        ? route.fulfill({ status: 503, body: 'unavailable' })
        : route.fallback(),
    )
    await page.reload()
    await expect(page.getByText('Imports unavailable', { exact: true })).toBeVisible()
    await page.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect(
      page.getByText(outcome === 'success' ? 'Session created:' : 'Request rejected.', {
        exact: false,
      }),
    ).toBeVisible()
    await expect(page.getByText('Imports unavailable', { exact: true })).toBeVisible()
    await expect(page.locator('.import-inspector')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0)
    await expect(page.getByRole('link', { name: /Settings/ })).not.toHaveAttribute(
      'aria-disabled',
      'true',
    )
    expect(requests).toHaveLength(2)
    expect(requests[1]).toBe(requests[0])
    expect(
      await page.evaluate(() => sessionStorage.getItem('signalbox.import-continuation.production')),
    ).toBeNull()
    expect(problems.pageErrors).toEqual([])
  })
}

test('retained continuation availability locks navigation before bootstrap and through failure', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.addInitScript(() => {
    sessionStorage.setItem(
      'signalbox.import-continuation.production',
      JSON.stringify({
        command_id: '00000000-0000-7000-8000-000000000001',
        frontier: {
          imported_conversation_id: '00000000-0000-7000-8000-000000000002',
          imported_entry_id: '00000000-0000-7000-8000-000000000003',
          position: 7,
        },
        relationship: 'resume',
        initial_model_selection: {
          kind: 'direct',
          selection_id: '00000000-0000-7000-8000-000000000004',
        },
      }),
    )
  })
  let releaseBootstrap: (() => void) | undefined
  const pendingBootstrap = new Promise<void>((resolve) => {
    releaseBootstrap = resolve
  })
  await page.route('**/api/bootstrap', async (route) => {
    await pendingBootstrap
    await route.abort()
  })
  await page.goto('/imports')
  const settings = page.getByRole('link', { name: 'Settings', exact: true })
  await expect(settings).toHaveAttribute('aria-disabled', 'true')
  await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette.getByRole('button', { name: /Go to Settings/ })).toHaveCount(0)
  if (testInfo.project.name === 'chromium')
    await expect(page).toHaveScreenshot('retained-continuation-pending-desktop.png')
  await page.keyboard.press('Escape')
  releaseBootstrap?.()
  await expect(page.getByRole('button', { name: 'Retry connection', exact: true })).toBeVisible()
  await expect(settings).toHaveAttribute('aria-disabled', 'true')
  await settings.click({ force: true })
  await expect(page).toHaveURL(/\/imports$/)
  await page.keyboard.press('g')
  await page.keyboard.press(',')
  await expect(page).toHaveURL(/\/imports$/)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.getByRole('button', { name: 'Retry connection', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Abandon', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Abandon', exact: true }).click()
  await expect(settings).not.toHaveAttribute('aria-disabled', 'true')
  await settings.click()
  await expect(page).toHaveURL(/\/settings$/)
})

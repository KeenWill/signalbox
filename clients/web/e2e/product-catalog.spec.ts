import bootstrapFixture from '../src/generated/web-contract-bootstrap.json' with { type: 'json' }
import { expect, type Page, type TestInfo, test } from './fontTest'

const firstSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const secondSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e'
const currentTurnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5d80'
const continuationSummaries = Array.from({ length: 30 }, (_, index) => ({
  action: null,
  active_turn_count: '0',
  archived: false,
  current_turn_id: null,
  goal_block: null,
  judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
  last_activity: { kind: 'session', unix_microseconds: String(1_724_194_799_999_000 - index) },
  queued_turn_count: '0',
  session_id: `018f1840-6f3d-7a8b-9c1d-${(0x0e2f3a4b5c80n + BigInt(index))
    .toString(16)
    .padStart(12, '0')}`,
  state: 'idle',
  title_summary: `Catalog session ${index + 3}`,
  title_truncated: false,
}))
const continuationBoundary = continuationSummaries.at(-1)
if (!continuationBoundary) throw new Error('catalog continuation fixture has a boundary')
const firstPage = {
  continuation: {
    kind: 'last_activity',
    session_id: continuationBoundary.session_id,
    unix_microseconds: continuationBoundary.last_activity.unix_microseconds,
  },
  cursor: '18',
  sort: 'last_activity_descending',
  summaries: [
    {
      action: null,
      active_turn_count: '1',
      archived: false,
      current_turn_id: currentTurnId,
      goal_block: null,
      judge: { actionable: '0', completed: '3', escalated: '0', failed: '0' },
      last_activity: { kind: 'turn', unix_microseconds: '1724200000000000' },
      queued_turn_count: '2',
      session_id: firstSessionId,
      state: 'active',
      title_summary: 'Release verification',
      title_truncated: false,
    },
    {
      action: 'provide_goal_need',
      active_turn_count: '0',
      archived: false,
      current_turn_id: null,
      goal_block: {
        generation: '4',
        need_summary: 'Select the authoritative deployment target.',
        reason: 'user_input_required',
      },
      judge: { actionable: '1', completed: '7', escalated: '1', failed: '0' },
      last_activity: { kind: 'goal', unix_microseconds: '1724194800000000' },
      queued_turn_count: '0',
      session_id: secondSessionId,
      state: 'blocked',
      title_summary: 'Deployment decision',
      title_truncated: false,
    },
    ...continuationSummaries,
  ],
  total: '48',
} as const
const filteredPage = {
  ...firstPage,
  continuation: null,
  summaries: [firstPage.summaries[0]],
  total: '1',
} as const
const secondPage = {
  continuation: null,
  cursor: '18',
  sort: 'last_activity_descending',
  summaries: [
    {
      action: null,
      active_turn_count: '0',
      archived: false,
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
      last_activity: { kind: 'session', unix_microseconds: '1724100000000000' },
      queued_turn_count: '0',
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c8f',
      state: 'idle',
      title_summary: 'A'.repeat(128),
      title_truncated: true,
    },
  ],
  total: '48',
} as const
const emptyAttentionPage = {
  continuation_after_session_id: null,
  cursor: '0',
  summaries: [],
} as const

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

test.beforeEach(async ({ page }) => {
  await page.route('**/api/sessions/rates?**', (route) =>
    route.fulfill({
      json: {
        sessions: new URL(route.request().url()).searchParams.getAll('session_id').map((id) => ({
          session_id: id,
          lifecycle_state: 'created',
          turn_count: '0',
          failed_turn_count: '0',
          retired_turn_count: '0',
          completed_turn_count: '0',
        })),
      },
    }),
  )
})

const useCatalogFixture = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: emptyAttentionPage })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => route.fulfill({ json: emptyAttentionPage }))
  await page.route('**/api/sessions/**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    const sessionId = decodeURIComponent(pathname.split('/')[3] ?? '')
    if (pathname.endsWith('/rates')) {
      const ids = new URL(route.request().url()).searchParams.getAll('session_id')
      return route.fulfill({
        json: {
          sessions: ids.map((id) => ({
            session_id: id,
            lifecycle_state:
              id === firstSessionId ? 'active' : id === secondSessionId ? 'parked' : 'terminal',
            turn_count: id === firstSessionId ? '10' : '2',
            failed_turn_count: id === secondSessionId ? '2' : '0',
            completed_turn_count: id === firstSessionId ? '7' : '0',
            retired_turn_count: '0',
            ...(id === secondSessionId
              ? {
                  last_failure_sequence: '123',
                  last_provider_cause: 'quota_exhausted',
                  goal_disposition: 'blocked',
                }
              : {}),
          })),
        },
      })
    }
    if (pathname.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: 78,
          continuation_before: null,
          continuation_after: null,
        },
      })
    }
    return route.fulfill({
      json: {
        session_id: sessionId,
        repository_watch: null,
        sizes: {
          item_count: '1',
          projected_text_bytes: '0',
          projected_structured_bytes: '78',
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: '41' },
        latest_address: { event_sequence: '41' },
        work: { active_turn_count: '1', queued_turn_count: '2' },
        observed_through: '41',
      },
    })
  })
  await page.route('**/api/sessions?**', (route) => {
    const request = new URL(route.request().url())
    const response = request.searchParams.has('after_session_id')
      ? secondPage
      : request.searchParams.has('search')
        ? filteredPage
        : firstPage
    return route.fulfill({ json: response })
  })
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('filters and opens a session with Enter, then returns to the catalog', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Search titles' }).fill('Release')
  await page.getByRole('textbox', { name: 'Search titles' }).press('Enter')
  await expect(page).toHaveURL(/q=Release/)
  await expect(page.getByRole('heading', { name: '1 session', exact: true })).toBeFocused()
  const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await session.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toHaveValue(
    firstSessionId,
  )
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
  await page.getByRole('textbox', { name: 'Session ID', exact: true }).press('Escape')
  await expect(page).toHaveURL(/q=Release/)
  await expect(session).toBeFocused()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('leaves the composer before returning to the catalog on Escape', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await session.click()
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
  const composer = page.getByRole('textbox', { name: 'Message to session', exact: true })
  const draft = 'Keep the draft while leaving the field.'
  await composer.fill(draft)
  await composer.press('Escape')
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toBeFocused()
  await expect(composer).toHaveValue(draft)
  await page.keyboard.press('Escape')
  await expect(session).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns browser Back focus to the row that opened a workspace', async ({
  page,
}, testInfo) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  const session = page.getByRole('button', { name: firstPage.summaries[1].title_summary })
  await session.click()
  const timeline = page.getByRole('listbox', { name: 'Session timeline' })
  await expect(timeline).toBeVisible()
  await timeline.focus()
  await page.goBack()
  await expect(session).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('catalog-return-focus.png') })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

for (const returnMethod of ['Escape', 'Back'] as const) {
  test(`retains catalog state and page order on ${returnMethod}`, async ({ page }) => {
    const problems = watchBrowser(page)
    await useCatalogFixture(page)
    await page.goto('/sessions')
    const lifecycle = page.getByRole('combobox', { name: /^State/ })
    const order = page.getByRole('combobox', { name: /^Page order/ })
    await lifecycle.selectOption('active')
    await order.selectOption('failure')
    const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
    await expect(
      page.getByRole('button', { name: firstPage.summaries[1].title_summary }),
    ).toHaveCount(0)
    await session.click()
    await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
    if (returnMethod === 'Escape')
      await page.getByRole('textbox', { name: 'Session ID', exact: true }).press('Escape')
    else await page.goBack()
    await expect(lifecycle).toHaveValue('active')
    await expect(order).toHaveValue('failure')
    await expect(session).toBeFocused()
    await expect(
      page.getByRole('button', { name: firstPage.summaries[1].title_summary }),
    ).toHaveCount(0)
    expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
  })
}

test('consumes return focus before a later catalog remount', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await session.click()
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
  await page.getByRole('textbox', { name: 'Session ID', exact: true }).press('Escape')
  await expect(session).toBeFocused()
  await page.getByRole('link', { name: /Settings/ }).click()
  const sessionsLink = page.getByRole('link', { name: /Sessions/ })
  await sessionsLink.click()
  await expect(session).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('shows unavailable timelines after opening a catalog row', async ({ page }, testInfo) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_session_timeline: false },
      },
    }),
  )
  let timelineReads = 0
  page.on('request', (request) => {
    if (new URL(request.url()).pathname.endsWith('/timeline')) timelineReads += 1
  })
  await page.goto('/sessions')
  await page.getByRole('button', { name: firstPage.summaries[0].title_summary }).click()
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toHaveValue(
    firstSessionId,
  )
  await page.screenshot({ path: testInfo.outputPath('timeline-unavailable.png') })
  await expect(page.getByRole('status').filter({ hasText: 'Sessions unavailable' })).toBeVisible()
  await expect(page.getByText('Loading session…', { exact: true })).toBeHidden()
  await expect(page.getByRole('button', { name: 'Open', exact: true })).toBeDisabled()
  expect(timelineReads).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves meaningful whitespace in exact catalog searches', async ({ page }) => {
  const problems = watchBrowser(page)
  let observedSearch: string | null = null
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) => {
    observedSearch = new URL(route.request().url()).searchParams.get('search')
    return route.fulfill({
      json:
        observedSearch === null
          ? firstPage
          : { ...firstPage, continuation: null, summaries: [], total: '0' },
    })
  })
  await page.goto('/sessions')

  const search = page.getByRole('textbox', { name: 'Search titles' })
  await search.fill(' release ')
  await search.press('Enter')
  await expect.poll(() => new URL(page.url()).searchParams.get('q')).toBe(' release ')
  await expect.poll(() => observedSearch).toBe(' release ')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('shows a visible focus indicator on catalog search', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  const search = page.getByRole('textbox', { name: 'Search titles' })
  await search.focus()

  await expect(search).toBeFocused()
  await expect
    .poll(() =>
      search.evaluate(
        (element) => getComputedStyle(element.parentElement as HTMLElement).outlineStyle,
      ),
    )
    .toBe('solid')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('rejects an over-bound search before changing URL state', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  await page.getByRole('textbox', { name: 'Search titles' }).fill('é'.repeat(513))
  await page.getByRole('button', { name: 'Apply' }).click()

  await expect(page.getByRole('alert')).toHaveText(/no more than 1,024 UTF-8 bytes/)
  await expect.poll(() => new URL(page.url()).searchParams.get('q')).toBeNull()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves the complete admitted title query on submit and reload', async ({ page }) => {
  await useCatalogFixture(page)
  const queries: string[] = []
  await page.route('**/api/sessions?**', (route) => {
    queries.push(new URL(route.request().url()).searchParams.get('search') ?? '')
    return route.fulfill({ json: { ...filteredPage, summaries: [], total: '0' } })
  })
  await page.goto('/sessions')
  const q = 'é'.repeat(512)
  await page.getByRole('textbox', { name: 'Search titles' }).fill(q)
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect.poll(() => queries.at(-1)).toBe(q)
  await expect.poll(() => new URL(page.url()).searchParams.get('q')).toBe(q)
  await page.reload()
  await expect(page.getByRole('textbox', { name: 'Search titles' })).toHaveValue(q)
  await expect.poll(() => queries.at(-1)).toBe(q)
})

test('restores focus after filters replace the bounded catalog page', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Search titles' }).fill('Release')
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect(page.getByRole('heading', { name: `${filteredPage.total} session` })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not defer focus restoration for unchanged filters', async ({ page }) => {
  const problems = watchBrowser(page)
  let sessionReads = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) => {
    sessionReads += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto('/sessions')

  const apply = page.getByRole('button', { name: 'Apply' })
  await apply.click()
  await page.context().setOffline(true)
  await page.context().setOffline(false)

  await expect.poll(() => sessionReads).toBeGreaterThan(1)
  await expect(apply).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus after a failed bounded catalog replacement', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) => {
    const request = new URL(route.request().url())
    return request.searchParams.has('search')
      ? route.fulfill({
          json: { invented: true },
        })
      : route.fulfill({ json: firstPage })
  })
  await page.goto('/sessions')

  await page.getByRole('textbox', { name: 'Search titles' }).fill('Release')
  await page.getByRole('button', { name: 'Apply' }).click()

  await expect(page.getByRole('heading', { name: 'Sessions could not be read' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps visible search synchronized when history distinguishes absent from undefined', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Search titles' }).fill('undefined')
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect(page.getByRole('textbox', { name: 'Search titles' })).toHaveValue('undefined')
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Search titles' })).toHaveValue('')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens the selected catalog row in the landed timeline workspace', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  await page.getByRole('button', { name: firstPage.summaries[0].title_summary }).click()

  await expect.poll(() => new URL(page.url()).searchParams.get('workspace')).toBe('true')
  await expect(page.getByRole('textbox', { name: 'Session ID' })).toHaveValue(firstSessionId)
  await expect(page.getByRole('textbox', { name: 'Session ID' })).toBeFocused()
  await expect(page.getByText(`Session workspace loaded for ${firstSessionId}.`)).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('replaces the bounded catalog page through its typed continuation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: `${firstPage.total} sessions` })).toBeVisible()

  const nextPage = page.getByRole('button', { name: 'Next page' })
  await nextPage.focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('button', { name: secondPage.summaries[0].title_summary }),
  ).toBeVisible()
  await expect(page).toHaveURL(/afterSession/)
  await expect(page.getByRole('button', { name: 'Next page' })).toBeHidden()
  await expect(page.getByRole('heading', { name: `${secondPage.total} sessions` })).toBeFocused()
  await expect(page.getByText('Truncated', { exact: true })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('gates catalog reads on a successful bootstrap', async ({ page }) => {
  const problems = watchBrowser(page)
  let sessionReads = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.route('**/api/sessions?**', (route) => {
    sessionReads += 1
    return route.fulfill({ json: firstPage })
  })

  await page.goto('/sessions')
  await expect(page.getByText('Contract rejected')).toBeVisible()
  await expect(page.getByText('Sessions unavailable')).toBeVisible()
  expect(sessionReads).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('recovers after retrying a transient bootstrap failure', async ({ page }) => {
  const problems = watchBrowser(page)
  let bootstrapReads = 0
  await page.route('**/api/bootstrap', (route) => {
    bootstrapReads += 1
    return bootstrapReads === 1
      ? route.fulfill({ json: { invented: true } })
      : route.fulfill({ json: bootstrapFixture })
  })
  await page.route('**/api/sessions?**', (route) => route.fulfill({ json: firstPage }))

  await page.goto('/sessions')
  await page.getByRole('button', { name: 'Retry sessions' }).click()
  await expect(page.getByRole('heading', { name: `${firstPage.total} sessions` })).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
  expect(bootstrapReads).toBe(2)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('exposes the server-owned blocked-goal need on its row', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(
    page.getByText('user input required · Select the authoritative deployment target.', {
      exact: true,
    }),
  ).toBeVisible()
  await expect(page.getByText('provide goal need', { exact: true })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses product navigation sequences and closes mobile navigation after activation', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  await page.keyboard.press('g')
  await page.keyboard.press('a')
  await expect(page).toHaveURL(/attention/)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.getByRole('button', { name: 'Open navigation' }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByRole('link', { name: 'Sessions' }).click()
  await expect(page).toHaveURL(/sessions/)
  await expect(dialog).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop dark, desktop light, and responsive catalog evidence', async ({
  page,
}, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: '48 sessions', exact: true })).toBeVisible()
  await expect(page.locator('.catalog-list li').first()).toContainText('10 turns')
  await expect.soft(page).toHaveScreenshot('catalog-desktop-dark.png', { animations: 'disabled' })

  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('catalog-desktop-light.png', { animations: 'disabled' })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  const row = page.locator('.catalog-list li').first().getByRole('button')
  const bounds = await row.boundingBox()
  const arrow = await row.locator('svg').boundingBox()
  expect(
    bounds && arrow && arrow.x >= bounds.x && arrow.x + arrow.width <= bounds.x + bounds.width,
  ).toBe(true)
  await expect.soft(page).toHaveScreenshot('catalog-mobile-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('listed outcomes expose failures and lifecycle filtering', async ({ page }) => {
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.locator('.catalog-list li').first()).toContainText('10 turns')
  await page.getByLabel('Page order').selectOption('failure')
  await expect(page.locator('.catalog-list li').first()).toContainText('Deployment decision')
  await expect(page.locator('.catalog-list li').first()).toContainText('quota exhausted')
  await page.getByLabel('State').selectOption('parked')
  await expect(page.locator('.catalog-list li')).toHaveCount(1)
  await expect(page.locator('.catalog-list li').first()).toContainText('2 turns · 2 failed')
})

test('catalog keyboard selection opens a session', async ({ page }) => {
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: '48 sessions', exact: true })).toBeVisible()
  await page.keyboard.press('j')
  await expect(page.locator('.catalog-list li').first().getByRole('button')).toBeFocused()
  await page.keyboard.press('j')
  await page.keyboard.press('Enter')
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toHaveValue(
    secondSessionId,
  )
})

test('keeps catalog rows and their order while outcomes are pending or unavailable', async ({
  page,
}) => {
  await useCatalogFixture(page)
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        ...firstPage,
        summaries: [
          { ...firstPage.summaries[0], session_id: secondSessionId },
          { ...firstPage.summaries[1], session_id: firstSessionId },
          ...firstPage.summaries.slice(2),
        ],
      },
    }),
  )
  let finish = () => {}
  const pending = new Promise<void>((resolve) => {
    finish = resolve
  })
  await page.route('**/api/sessions/rates?**', async (route) => {
    await pending
    return route.fulfill({
      status: 503,
      json: {
        error: { kind: 'application', code: 'unavailable', message: 'Outcomes unavailable' },
      },
    })
  })
  await page.goto('/sessions')
  await expect(page.locator('.catalog-list li')).toHaveCount(32)
  const rows = await page.locator('.catalog-list li strong').allTextContents()
  await page.getByLabel('Page order').selectOption('failure')
  await page.getByLabel('State').selectOption('parked')
  await expect(page.locator('.catalog-list li')).toHaveCount(32)
  await expect(page.locator('.catalog-list li strong')).toHaveText(rows)
  finish()
  await expect(page.getByText('Session outcomes unavailable.')).toBeVisible()
  await expect(page.locator('.catalog-list li strong')).toHaveText(rows)
  await expect(page.locator('.catalog-list li')).toHaveCount(32)
})

test('replaces catalog continuation history instead of accumulating visited pages', async ({
  page,
}) => {
  await useCatalogFixture(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: 'Sessions' }).click()
  await page.getByRole('button', { name: 'Next page' }).click()
  await expect(page).toHaveURL(/afterSession=/)
  await page.goBack()
  await expect(page).toHaveURL(/\/attention$/)
})

test('keeps opened workspace identities in the URL and restores them on reload', async ({
  page,
}) => {
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('button', { name: 'Open by ID' }).click()
  const input = page.getByRole('textbox', { name: 'Session ID' })
  await expect(input).toBeFocused()
  await input.fill(firstSessionId)
  await input.press('Enter')
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await expect(page.getByText(`Session workspace loaded for ${firstSessionId}.`)).toBeVisible()
  await input.fill(secondSessionId)
  await input.press('Enter')
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(secondSessionId)
  await expect(page.getByText(`Session workspace loaded for ${secondSessionId}.`)).toBeVisible()
  await page.reload()
  await expect(input).toHaveValue(secondSessionId)
  await expect(page.getByText(`Session workspace loaded for ${secondSessionId}.`)).toBeVisible()
})

test('classifies a catalog connection failure as transport unavailability', async ({ page }) => {
  await useCatalogFixture(page)
  await page.route('**/api/sessions?**', (route) => route.abort())
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: 'Sessions could not be read' })).toBeVisible()
  await expect(page.getByRole('alert')).not.toContainText('generated web contract')
  await expect(page.getByRole('alert')).toContainText('The Signalbox daemon could not be reached.')
})

test('opens a session link directly on a phone without an inspector', async ({ page }) => {
  await useCatalogFixture(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/sessions?session=${firstSessionId}`)
  await expect(page.getByRole('textbox', { name: 'Session ID', exact: true })).toHaveValue(
    firstSessionId,
  )
  await expect(page.getByRole('dialog')).toHaveCount(0)
})

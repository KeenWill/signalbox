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
  repository_watch: null,
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
      repository_watch: null,
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
      repository_watch: null,
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
      repository_watch: null,
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

const useCatalogFixture = async (page: Page, titleGeneration = true) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: {
          ...bootstrapFixture.capabilities,
          session_title_generation: titleGeneration,
        },
      },
    }),
  )
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
    if (pathname.endsWith('/timeline-detail'))
      return route.fulfill({
        json: { session_id: sessionId, items: [], projected_body_bytes: 0, continuation: null },
      })
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
        supervision: null,
        repository_watch: null,
        workspace_root_kind: null,
        title_summary: null,
        last_activity: { kind: 'session', unix_microseconds: '1' },
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
      : request.searchParams.get('include_archived') === 'true'
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
  await page.getByRole('checkbox', { name: 'Include archived' }).check()
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect(page).toHaveURL(/archived=true/)
  await expect(page.getByRole('heading', { name: '1 session', exact: true })).toBeFocused()
  const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await session.focus()
  await page.keyboard.press('Enter')
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
  await page.getByRole('region', { name: 'Conversation', exact: true }).press('Escape')
  await expect(page).toHaveURL(/archived=true/)
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
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
  const composer = page.getByRole('textbox', { name: 'Message', exact: true })
  const draft = 'Keep the draft while leaving the field.'
  await composer.fill(draft)
  await composer.press('Escape')
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeFocused()
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
  const timeline = page.getByRole('region', { name: 'Conversation', exact: true })
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
    await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
    if (returnMethod === 'Escape')
      await page.getByRole('region', { name: 'Conversation', exact: true }).press('Escape')
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
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
  await page.getByRole('region', { name: 'Conversation', exact: true }).press('Escape')
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
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await page.screenshot({ path: testInfo.outputPath('timeline-unavailable.png') })
  await expect(
    page.getByRole('status').filter({ hasText: 'Session timeline unavailable' }),
  ).toBeVisible()
  await expect(page.getByText('Loading session…', { exact: true })).toBeHidden()
  await expect(page.getByRole('button', { name: 'Open', exact: true })).toHaveCount(0)
  expect(timelineReads).toBe(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('searches conversation content with a trimmed query', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  let observedSearch: string | null = null
  const catalogQueries: (string | null)[] = []
  page.on('request', (request) => {
    const url = new URL(request.url())
    if (url.pathname === '/api/sessions') catalogQueries.push(url.searchParams.get('search'))
  })
  await page.route('**/api/search?**', (route) => {
    observedSearch = new URL(route.request().url()).searchParams.get('q')
    return route.fulfill({ json: { results: [], continuation: null } })
  })
  await page.goto('/sessions')
  const search = page.getByRole('textbox', { name: 'Search conversations' })
  await search.fill(' release ')
  await search.press('Enter')
  await expect(page).toHaveURL(/\/search\?q=release/)
  await expect.poll(() => observedSearch).toBe('release')
  expect(catalogQueries.length).toBeGreaterThan(0)
  expect(catalogQueries.every((query) => query === null)).toBe(true)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('shows a visible focus indicator on catalog search', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  const search = page.getByRole('textbox', { name: 'Search conversations' })
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
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_search_query_bytes: 8 },
      },
    }),
  )
  await page.goto('/sessions')

  await page.getByRole('textbox', { name: 'Search conversations' }).fill('é'.repeat(5))
  await page.getByRole('button', { name: 'Apply' }).click()

  await expect(page.getByRole('alert')).toHaveText('Check your search. Try a shorter phrase.')
  await expect.poll(() => new URL(page.url()).searchParams.get('q')).toBeNull()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('hides conversation search when lexical search is unavailable', async ({ page }) => {
  await useCatalogFixture(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: {
          ...bootstrapFixture.capabilities,
          bounded_lexical_search: false,
        },
      },
    }),
  )
  let searchReads = 0
  page.on('request', (request) => {
    if (new URL(request.url()).pathname === '/api/search') searchReads += 1
  })
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: '48 sessions', exact: true })).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Search conversations' })).toHaveCount(0)
  await page.getByRole('checkbox', { name: 'Include archived' }).check()
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect(page).toHaveURL(/archived=true/)
  expect(searchReads).toBe(0)
})

test('preserves the complete admitted conversation query on submit and reload', async ({
  page,
}) => {
  await useCatalogFixture(page)
  const queries: string[] = []
  await page.route('**/api/search?**', (route) => {
    queries.push(new URL(route.request().url()).searchParams.get('q') ?? '')
    return route.fulfill({ json: { results: [], continuation: null } })
  })
  await page.goto('/sessions')
  const q = 'é'.repeat(256)
  await page.getByRole('textbox', { name: 'Search conversations' }).fill(q)
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect.poll(() => queries.at(-1)).toBe(q)
  await expect.poll(() => new URL(page.url()).searchParams.get('q')).toBe(q)
  await page.reload()
  await expect(page.getByRole('textbox', { name: 'Search text', exact: true })).toHaveValue(q)
  await expect.poll(() => queries.at(-1)).toBe(q)
})

test('restores focus after filters replace the bounded catalog page', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Include archived' }).check()
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
    return request.searchParams.get('include_archived') === 'true'
      ? route.fulfill({
          json: { invented: true },
        })
      : route.fulfill({ json: firstPage })
  })
  await page.goto('/sessions')

  await page.getByRole('checkbox', { name: 'Include archived' }).check()
  await page.getByRole('button', { name: 'Apply' }).click()

  await expect(page.getByRole('heading', { name: 'Sessions failed to load' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('keeps visible search synchronized when history distinguishes absent from undefined', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.route('**/api/search?**', (route) =>
    route.fulfill({ json: { results: [], continuation: null } }),
  )
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Search conversations' }).fill('undefined')
  await page.getByRole('button', { name: 'Apply' }).click()
  await expect(page.getByRole('textbox', { name: 'Search text', exact: true })).toHaveValue(
    'undefined',
  )
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Search conversations' })).toHaveValue('')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens the selected catalog row in the landed timeline workspace', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')

  await page.getByRole('button', { name: firstPage.summaries[0].title_summary }).click()

  await expect.poll(() => new URL(page.url()).searchParams.get('workspace')).toBe('true')
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
  await page.keyboard.press('Escape')
  const row = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await expect(row).toBeFocused()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
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
  await expect(page.getByText('Unexpected daemon response')).toBeVisible()
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
  await expect(page.getByRole('button', { name: 'Retry connection', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: /^Retry/ })).toHaveCount(1)
  await page.getByRole('button', { name: 'Retry connection', exact: true }).click()
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
    page.getByText('Blocked: Input required — Select the authoritative deployment target.', {
      exact: true,
    }),
  ).toBeVisible()
  await expect(page.getByText('Input required', { exact: true })).toBeVisible()
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
  const row = page.getByRole('button', { name: /Release verification/ })
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
  await expect(page.locator('.catalog-list li').first()).toContainText('Quota reached')
  await page.getByLabel('State').selectOption('parked')
  await expect(page.locator('.catalog-list li')).toHaveCount(1)
  await expect(page.locator('.catalog-list li').first()).toContainText('2 turns · 2 failed')
})

test('catalog keyboard selection opens a session', async ({ page }) => {
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: '48 sessions', exact: true })).toBeVisible()
  await page.keyboard.press('j')
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeFocused()
  await page.keyboard.press('j')
  await page.keyboard.press('Enter')
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(secondSessionId)
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

for (const entry of ['direct', 'reload'] as const) {
  test(`offers the catalog from an empty workspace after bootstrap on ${entry}`, async ({
    page,
  }) => {
    await useCatalogFixture(page)
    const input = page.getByRole('button', { name: 'Choose a session', exact: true })
    if (entry === 'reload') {
      await page.goto('/sessions?workspace=true')
      await expect(input).toBeVisible()
    }
    const bootstrapReady = Promise.withResolvers<void>()
    await page.route('**/api/bootstrap', async (route) => {
      await bootstrapReady.promise
      await route.fulfill({ json: bootstrapFixture })
    })
    try {
      if (entry === 'reload') await page.reload()
      else await page.goto('/sessions?workspace=true')
      await expect(page.getByRole('main')).toBeFocused()
      await expect(input).toHaveCount(0)
      bootstrapReady.resolve()
      await expect(input).toBeVisible()
      await input.click()
      await page.getByRole('button', { name: firstPage.summaries[0].title_summary }).click()
      await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
      await expect(page.getByRole('main')).toBeFocused()
    } finally {
      bootstrapReady.resolve()
    }
  })
}

test('keeps opened workspace identities in the URL and restores them on reload', async ({
  page,
}) => {
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await page.getByRole('button', { name: firstPage.summaries[0].title_summary }).click()
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await page.getByRole('region', { name: 'Conversation', exact: true }).press('Escape')
  await page.getByRole('button', { name: firstPage.summaries[1].title_summary }).click()
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(secondSessionId)
  await page.reload()
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(secondSessionId)
  await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
})

test('classifies a catalog connection failure as transport unavailability', async ({ page }) => {
  await useCatalogFixture(page)
  await page.route('**/api/sessions?**', (route) => route.abort())
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: 'Sessions failed to load' })).toBeVisible()
  await expect(page.getByRole('alert')).not.toContainText('generated web contract')
  await expect(page.getByRole('alert')).toContainText('Signalbox daemon unreachable.')
})

test('opens a session link directly on a phone without an inspector', async ({ page }) => {
  await useCatalogFixture(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/sessions?session=${firstSessionId}`)
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(firstSessionId)
  await expect(page.getByRole('dialog')).toHaveCount(0)
})

test('keeps untitled session fallbacks without the detail capability', async ({ page }) => {
  await useCatalogFixture(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_session_timeline_detail: false },
      },
    }),
  )
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        ...firstPage,
        summaries: firstPage.summaries.map((summary, index) =>
          index === 0 ? { ...summary, title_summary: null } : summary,
        ),
      },
    }),
  )
  let timelineReads = 0
  page.on('request', (request) => {
    if (new URL(request.url()).pathname.includes('/timeline')) timelineReads += 1
  })
  await page.goto('/sessions')
  await expect(
    page.getByRole('button', { name: new RegExp(`Session ${firstSessionId}`) }),
  ).toBeVisible()
  expect(timelineReads).toBe(0)
})

test('renames a session without randomUUID and reads back the saved catalog title', async ({
  page,
}) => {
  await page.addInitScript(() => {
    Object.defineProperty(crypto, 'randomUUID', { value: undefined })
  })
  await useCatalogFixture(page)
  let title = firstPage.summaries[0].title_summary as string
  const requests: Array<{ command_id: string; title: string }> = []
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        ...firstPage,
        summaries: firstPage.summaries.map((row, index) =>
          index === 0 ? { ...row, title_summary: title } : row,
        ),
      },
    }),
  )
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    expect(route.request().method()).toBe('PATCH')
    requests.push(route.request().postDataJSON())
    title = requests.at(-1)?.title ?? title
    await route.fulfill({ status: 204 })
  })
  await page.goto('/sessions')
  const rename = page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true })
  await rename.click()
  const input = page.getByRole('textbox', { name: 'Session title', exact: true })
  await expect(input).toBeFocused()
  await input.fill('Ship the release')
  await input.press('Enter')
  await expect(page.getByRole('button', { name: /Ship the release/ })).toBeVisible()
  await expect(rename).toBeFocused()
  expect(requests).toHaveLength(1)
  expect(requests[0]?.command_id).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  )
  await page.reload()
  await expect(page.getByRole('button', { name: /Ship the release/ })).toBeVisible()
})

test('retries a failed rename with the same intent and can cancel editing', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(crypto, 'randomUUID', { value: undefined })
  })
  await useCatalogFixture(page)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    requests.push(route.request().postDataJSON())
    await route.abort('failed')
  })
  await page.goto('/sessions')
  const rename = page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true })
  await rename.click()
  const input = page.getByRole('textbox', { name: 'Session title', exact: true })
  await input.fill('Cancel this title')
  await input.press('Escape')
  await expect(rename).toBeFocused()
  await rename.click()
  await input.fill('Retry this title')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByRole('alert')).toBeVisible()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(2)
  await expect(page.getByRole('alert')).toBeVisible()
  expect(requests[1]).toEqual(requests[0])
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(rename).toBeFocused()
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
})

test('links catalog pull request and trigger chips and retains a PR title fallback', async ({
  page,
}, testInfo) => {
  await useCatalogFixture(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_session_timeline_detail: false },
      },
    }),
  )
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        ...firstPage,
        continuation: null,
        total: '1',
        summaries: [
          {
            ...firstPage.summaries[0],
            title_summary: null,
            repository_watch: {
              repository: 'signalbox/example',
              pull_request: '81',
              event_kind: 'review_submitted',
              head_branch: 'review',
              base_branch: 'main',
              action_ordinal: '2',
              dispatch_id: '00000000-0000-0000-0000-000000000063',
              event_id: '00000000-0000-0000-0000-000000000064',
              rule_id: 'review-response',
              rule_revision: '3',
            },
          },
        ],
      },
    }),
  )
  await page.goto('/sessions')
  await expect(page.getByRole('button', { name: /PR #81/ })).toBeVisible()
  await expect(page.getByRole('link', { name: 'PR #81', exact: true })).toHaveAttribute(
    'href',
    'https://github.com/signalbox/example/pull/81',
  )
  await expect(page.getByRole('link', { name: 'Review submitted', exact: true })).toHaveAttribute(
    'href',
    'https://github.com/signalbox/example/pull/81',
  )
  await page.screenshot({ path: testInfo.outputPath('catalog-provenance.png') })
  await page.setViewportSize({ width: 390, height: 844 })
  await page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true }).click()
  await page.getByRole('textbox', { name: 'Session title', exact: true }).fill('Review response')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  )
  await page.screenshot({ path: testInfo.outputPath('catalog-rename-mobile.png') })
})

for (const control of ['Save', 'Cancel']) {
  test(`Escape cancels rename from ${control}`, async ({ page }) => {
    await useCatalogFixture(page)
    await page.goto('/sessions')
    const rename = page.getByRole('button', {
      name: `Rename session ${firstSessionId}`,
      exact: true,
    })
    await rename.click()
    await page.getByRole('button', { name: control, exact: true }).focus()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('textbox', { name: 'Session title', exact: true })).toHaveCount(0)
    await expect(rename).toBeFocused()
  })
}

test('shows the daemon rename rejection and retains the retry identity', async ({ page }) => {
  await useCatalogFixture(page)
  const requests: unknown[] = []
  const message = 'The rename outcome is unconfirmed. Retry with the same command ID.'
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    requests.push(route.request().postDataJSON())
    await route.fulfill({
      status: 503,
      json: {
        error: {
          kind: 'application',
          code: 'unconfirmed',
          message,
        },
      },
    })
  })
  await page.goto('/sessions')
  await page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true }).click()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByRole('alert')).toHaveText(message)
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(2)
  await expect(page.getByRole('alert')).toHaveText(message)
  expect(requests[1]).toEqual(requests[0])
})

test('a stalled rename releases the form and retries the same intent', async ({ page }) => {
  await useCatalogFixture(page)
  await page.clock.install()
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    requests.push(route.request().postDataJSON())
    if (requests.length > 1) await route.abort('failed')
  })
  await page.goto('/sessions')
  const rename = page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true })
  await rename.click()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(1)
  await page.clock.fastForward(30_000)
  await expect(page.getByRole('alert')).toHaveText(
    'Rename timed out. Retry to confirm the same title.',
  )
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeEnabled()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(2)
  await expect(page.getByRole('alert')).toBeVisible()
  expect(requests[1]).toEqual(requests[0])
  await page.getByRole('button', { name: 'Cancel', exact: true }).press('Escape')
  await expect(rename).toBeFocused()
})

test('releases the rename editor while the acknowledged catalog refresh is stalled', async ({
  page,
}) => {
  await useCatalogFixture(page)
  const requests: Array<{ command_id: string; title: string }> = []
  let reads = 0
  await page.route('**/api/sessions?**', async (route) => {
    reads += 1
    if (reads === 1) await route.fulfill({ json: firstPage })
  })
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await page.goto('/sessions')
  const rename = page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true })
  await rename.click()
  await page.getByRole('textbox', { name: 'Session title', exact: true }).fill('Acknowledged title')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => reads).toBe(2)
  await expect(page.getByRole('textbox', { name: 'Session title', exact: true })).toHaveCount(0)
  await expect(rename).toBeEnabled()
  await expect(rename).toBeFocused()
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
  await rename.click()
  await expect(page.getByRole('textbox', { name: 'Session title', exact: true })).toHaveValue(
    'Acknowledged title',
  )
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(2)
  expect(requests.map((request) => request.title)).toEqual([
    'Acknowledged title',
    'Acknowledged title',
  ])
  expect(requests[1]?.command_id).toBe(requests[0]?.command_id)
})

for (const [leaveCatalog, firstRequestArrived, concurrentOutcome] of [
  [false, true, 'none'],
  [true, true, 'none'],
  [true, false, 'none'],
  [true, true, 'failure'],
  [true, true, 'success'],
] as const) {
  test(`replays an ambiguous rename after editor closure with catalog unmount ${leaveCatalog} and first request delivered ${firstRequestArrived} and concurrent retry outcome ${concurrentOutcome}`, async ({
    page,
  }) => {
    await useCatalogFixture(page)
    const requests: Array<{ command_id: string; title: string }> = []
    let savedTitle = 'Release verification'
    let stallRefresh = false
    let refreshStalled = false
    let releaseRefresh: (() => Promise<void>) | undefined
    let completeRetry: (() => Promise<void>) | undefined
    await page.route('**/api/sessions?**', (route) => {
      const fulfill = () =>
        route.fulfill({
          json: {
            ...firstPage,
            summaries: firstPage.summaries.map((row, index) =>
              index === 0 ? { ...row, title_summary: savedTitle } : row,
            ),
          },
        })
      if (stallRefresh) {
        refreshStalled = true
        releaseRefresh = fulfill
        return
      }
      return fulfill()
    })
    await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
      requests.push(route.request().postDataJSON())
      if (concurrentOutcome !== 'none' && requests.length === 3) {
        completeRetry = () =>
          concurrentOutcome === 'failure' ? route.abort('failed') : route.fulfill({ status: 204 })
        return
      }
      if (requests.length === 1) {
        if (!firstRequestArrived) {
          await route.abort('failed')
          return
        }
        savedTitle = 'Another writer title'
        await route.fulfill({
          status: 503,
          json: {
            error: {
              kind: 'application',
              code: 'metadata_outcome_unconfirmed',
              message: 'Retry the same title and identity.',
            },
          },
        })
      } else {
        if (requests.at(-1)?.command_id !== requests[0]?.command_id)
          savedTitle = requests.at(-1)?.title ?? savedTitle
        if (!firstRequestArrived) savedTitle = 'My ambiguous title'
        stallRefresh = leaveCatalog
        await route.fulfill({ status: 204 })
      }
    })
    await page.goto('/sessions')
    const rename = page.getByRole('button', {
      name: `Rename session ${firstSessionId}`,
      exact: true,
    })
    await rename.click()
    const input = page.getByRole('textbox', { name: 'Session title', exact: true })
    await input.fill('My ambiguous title')
    await page.getByRole('button', { name: 'Save', exact: true }).click()
    await expect(page.getByRole('alert')).toBeVisible()
    await page.getByRole('button', { name: 'Cancel', exact: true }).click()
    if (leaveCatalog) {
      await page.getByRole('link', { name: 'Settings', exact: true }).click()
      await expect(page).toHaveURL(/\/settings$/)
      await page.getByRole('link', { name: 'Sessions', exact: true }).click()
      await expect(
        page.getByRole('button', {
          name: firstRequestArrived ? /Another writer title/ : /Release verification/,
        }),
      ).toBeVisible()
    }
    await rename.click()
    await expect(input).toHaveValue('My ambiguous title')
    await expect(input).toHaveAttribute('readonly', '')
    await page.getByRole('button', { name: 'Save', exact: true }).click()
    await expect(input).toBeHidden()
    if (leaveCatalog) await expect.poll(() => refreshStalled).toBe(true)
    else await expect(page.getByRole('button', { name: /Another writer title/ })).toBeVisible()
    expect(requests).toHaveLength(2)
    expect(requests[1]).toEqual(requests[0])
    await rename.click()
    if (leaveCatalog) {
      await expect(input).toHaveAttribute('readonly', '')
      await expect(input).toHaveValue('My ambiguous title')
      await expect(page.getByRole('alert')).toContainText('Rename acknowledged')
      if (concurrentOutcome !== 'none') {
        await page.getByRole('button', { name: 'Save', exact: true }).click()
        await expect.poll(() => requests.length).toBe(3)
        expect(requests[2]).toEqual(requests[0])
        await expect(input).toBeDisabled()
      }
      const refreshed = page.waitForResponse((response) =>
        response.url().includes('/api/sessions?'),
      )
      await releaseRefresh?.()
      await (await refreshed).finished()
      await expect(
        page.getByRole('button', {
          name: firstRequestArrived ? /Another writer title/ : /My ambiguous title/,
        }),
      ).toBeVisible()
      if (concurrentOutcome !== 'none') {
        await expect(input).toHaveValue('Another writer title')
        const previousRefresh = releaseRefresh
        await completeRetry?.()
        if (concurrentOutcome === 'success') {
          await expect(input).toBeHidden()
          await expect.poll(() => releaseRefresh !== previousRefresh).toBe(true)
          await rename.click()
          await expect(input).toHaveAttribute('readonly', '')
          await expect(input).toHaveValue('My ambiguous title')
          await expect(page.getByRole('alert')).toContainText('Rename acknowledged')
          await releaseRefresh?.()
        }
      }
    }
    await expect(input).toBeEditable()
    const confirmedTitle = firstRequestArrived ? 'Another writer title' : 'My ambiguous title'
    await expect(input).toHaveValue(confirmedTitle)
    await page.getByRole('button', { name: 'Save', exact: true }).click()
    await expect.poll(() => requests.length).toBe(concurrentOutcome !== 'none' ? 4 : 3)
    expect(requests.at(-1)?.title).toBe(confirmedTitle)
    expect(requests.at(-1)?.command_id).not.toBe(requests[0]?.command_id)
    await expect(input).toBeHidden()
    await expect(page.getByRole('button', { name: new RegExp(confirmedTitle) })).toBeVisible()
  })
}

test('a 413 rename rejection permits a corrected title with a new identity', async ({ page }) => {
  await useCatalogFixture(page)
  const requests: Array<{ command_id: string; title: string }> = []
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    requests.push(route.request().postDataJSON())
    await route.fulfill({
      status: 413,
      json: {
        error: {
          kind: 'application',
          code: 'invalid_session_title',
          message: 'Title does not fit the metadata size limit.',
        },
      },
    })
  })
  await page.goto('/sessions')
  await page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true }).click()
  const input = page.getByRole('textbox', { name: 'Session title', exact: true })
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByRole('alert')).toContainText('metadata size limit')
  await expect(input).toBeEditable()
  await input.fill('Corrected title')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect.poll(() => requests.length).toBe(2)
  expect(requests[1]?.command_id).not.toBe(requests[0]?.command_id)
})

for (const finish of ['Save', 'Cancel', 'Escape']) {
  test(`rename selects its row for keyboard navigation after ${finish}`, async ({ page }) => {
    await useCatalogFixture(page)
    await page.route(`**/api/sessions/${secondSessionId}/metadata`, (route) =>
      route.fulfill({ status: 204 }),
    )
    await page.goto('/sessions')
    await expect(page.getByRole('heading', { name: '48 sessions', exact: true })).toBeVisible()
    await page.keyboard.press('j')
    await expect(page.getByRole('button', { name: /Release verification/ })).toBeFocused()
    const rename = page.getByRole('button', {
      name: `Rename session ${secondSessionId}`,
      exact: true,
    })
    await rename.click()
    const input = page.getByRole('textbox', { name: 'Session title', exact: true })
    await expect(input).toBeFocused()
    if (finish === 'Escape') await input.press('Escape')
    else await page.getByRole('button', { name: finish, exact: true }).click()
    await expect(rename).toBeFocused()
    await page.keyboard.press('j')
    await expect(page.getByRole('button', { name: /^Catalog session 3 / })).toBeFocused()
    await page.keyboard.press('Enter')
    await expect
      .poll(() => new URL(page.url()).searchParams.get('session'))
      .toBe(continuationSummaries[0]?.session_id)
  })
}

test('disables suggestions in the row and palette when title generation is unavailable', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page, false)
  await page.goto('/sessions')
  await expect(
    page.getByRole('button', { name: `Suggest a name for session ${firstSessionId}`, exact: true }),
  ).toBeDisabled()
  await expect(
    page.getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true }),
  ).toBeEnabled()
  await page.getByRole('button', { name: /Release verification/ }).focus()
  await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette.getByRole('button', { name: /Suggest a name/ })).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suggests a title inline and saves only after keyboard acceptance', async ({
  page,
}, testInfo) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  const patches: Array<{ command_id: string; title: string }> = []
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, async (route) => {
    expect(route.request().method()).toBe('POST')
    expect(route.request().postDataJSON()).toEqual({})
    await route.fulfill({ json: { title: 'Verify the release' } })
  })
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    patches.push(route.request().postDataJSON())
    await route.fulfill({ status: 204 })
  })
  await page.goto('/sessions')
  const suggest = page.getByRole('button', {
    name: `Suggest a name for session ${firstSessionId}`,
    exact: true,
  })
  await suggest.focus()
  await page.keyboard.press('Enter')
  const accept = page.getByRole('button', { name: 'Accept', exact: true })
  await expect(accept).toBeFocused()
  await expect(page.getByText('Verify the release', { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
  expect(patches).toEqual([])
  const desktop = testInfo.outputPath('suggested-title-desktop-dark.png')
  await page.screenshot({ path: desktop })
  await testInfo.attach('suggested-title-desktop-dark', { path: desktop, contentType: 'image/png' })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  const light = testInfo.outputPath('suggested-title-desktop-light.png')
  await page.screenshot({ path: light })
  await testInfo.attach('suggested-title-desktop-light', { path: light, contentType: 'image/png' })
  await page.setViewportSize({ width: 390, height: 844 })
  const phone = testInfo.outputPath('suggested-title-phone.png')
  await page.screenshot({ path: phone })
  await testInfo.attach('suggested-title-phone', { path: phone, contentType: 'image/png' })
  await accept.press('Enter')
  await expect(accept).toBeHidden()
  await expect(suggest).toBeFocused()
  expect(patches).toHaveLength(1)
  expect(patches[0]?.title).toBe('Verify the release')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('edits a suggested title before saving and preserves rename retry identity', async ({
  page,
}) => {
  await useCatalogFixture(page)
  const patches: Array<{ command_id: string; title: string }> = []
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, (route) =>
    route.fulfill({ json: { title: 'Verify the release' } }),
  )
  await page.route(`**/api/sessions/${firstSessionId}/metadata`, async (route) => {
    patches.push(route.request().postDataJSON())
    if (patches.length === 1) {
      await route.fulfill({
        status: 503,
        json: {
          error: {
            kind: 'application',
            code: 'metadata_outcome_unconfirmed',
            message: 'Retry to confirm the title.',
          },
        },
      })
    } else await route.fulfill({ status: 204 })
  })
  await page.goto('/sessions')
  const suggest = page.getByRole('button', {
    name: `Suggest a name for session ${firstSessionId}`,
    exact: true,
  })
  await suggest.click()
  await page.getByRole('button', { name: 'Edit', exact: true }).click()
  const input = page.getByRole('textbox', { name: 'Session title', exact: true })
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('Verify the release')
  await input.fill('Release checklist review')
  await input.press('Enter')
  await expect(page.getByRole('alert')).toContainText('Retry to confirm the title.')
  await expect(input).toHaveAttribute('readonly', '')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(input).toBeHidden()
  await expect(suggest).toBeFocused()
  expect(patches).toHaveLength(2)
  expect(patches[0]?.title).toBe('Release checklist review')
  expect(patches[1]).toEqual(patches[0])
})

test('reports suggestion failures without changing the title and permits retry', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, (route) =>
    route.fulfill({
      status: 503,
      json: {
        error: {
          kind: 'application',
          code: 'session_title_generation_failed',
          message: 'A name could not be suggested. Try again.',
        },
      },
    }),
  )
  await page.goto('/sessions')
  const suggest = page.getByRole('button', {
    name: `Suggest a name for session ${firstSessionId}`,
    exact: true,
  })
  await suggest.click()
  await expect(page.getByRole('alert')).toContainText('A name could not be suggested. Try again.')
  await expect(page.getByRole('button', { name: 'Accept', exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel', exact: true }).press('Escape')
  await expect(suggest).toBeFocused()
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, (route) =>
    route.fulfill({ json: { title: 'Verify the release' } }),
  )
  await suggest.click()
  await expect(page.getByRole('button', { name: 'Accept', exact: true })).toBeFocused()
  await page.getByRole('button', { name: 'Accept', exact: true }).press('Escape')
  await expect(suggest).toBeFocused()
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.filter((message) => !message.includes('503'))).toEqual([])
})

test('rejects a malformed suggestion instead of offering to save it', async ({ page }) => {
  await useCatalogFixture(page)
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, (route) =>
    route.fulfill({ json: { title: '' } }),
  )
  await page.goto('/sessions')
  await page
    .getByRole('button', { name: `Suggest a name for session ${firstSessionId}`, exact: true })
    .click()
  await expect(page.getByRole('alert')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Accept', exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: /Release verification/ })).toBeVisible()
})

test('cancels a pending suggestion without reopening the editor when it completes', async ({
  page,
}) => {
  await useCatalogFixture(page)
  const pending = Promise.withResolvers<void>()
  const answered = Promise.withResolvers<void>()
  await page.route(`**/api/sessions/${firstSessionId}/title/suggest`, async (route) => {
    await pending.promise
    await route.fulfill({ json: { title: 'Late generated title' } })
    answered.resolve()
  })
  await page.goto('/sessions')
  const suggest = page.getByRole('button', {
    name: `Suggest a name for session ${firstSessionId}`,
    exact: true,
  })
  try {
    await suggest.click()
    const cancel = page.getByRole('button', { name: 'Cancel', exact: true })
    await expect(cancel).toBeFocused()
    await expect(page.getByText('Suggesting a name…', { exact: true })).toBeVisible()
    await cancel.press('Escape')
    await expect(suggest).toBeFocused()
    pending.resolve()
    await answered.promise
    await page
      .getByRole('button', { name: `Rename session ${firstSessionId}`, exact: true })
      .click()
    await expect(page.getByRole('textbox', { name: 'Session title', exact: true })).toHaveValue(
      'Release verification',
    )
    await expect(page.getByText('Late generated title', { exact: true })).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Accept', exact: true })).toHaveCount(0)
  } finally {
    pending.resolve()
  }
})

test('suggests a name for the keyboard-selected session through the command palette', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  const suggestions: string[] = []
  const patches: string[] = []
  await page.route('**/api/sessions/*/title/suggest', async (route) => {
    suggestions.push(route.request().url())
    await route.fulfill({ json: { title: 'Decide the deployment' } })
  })
  await page.route('**/api/sessions/*/metadata', async (route) => {
    patches.push(route.request().url())
    await route.fulfill({ status: 204 })
  })
  await page.goto('/sessions')
  await page.getByRole('button', { name: /Deployment decision/ }).focus()
  await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await palette.getByRole('button', { name: /Suggest a name/ }).focus()
  await page.keyboard.press('Enter')
  await expect(palette).toBeHidden()
  await expect(page.getByRole('button', { name: 'Accept', exact: true })).toBeFocused()
  expect(suggestions).toHaveLength(1)
  expect(suggestions[0]).toContain(`/sessions/${secondSessionId}/title/suggest`)
  expect(patches).toEqual([])
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(
    page.getByRole('button', {
      name: `Suggest a name for session ${secondSessionId}`,
      exact: true,
    }),
  ).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

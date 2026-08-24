import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'

const sessionId = (index: number) =>
  `00000000-0000-0000-0000-${index.toString(16).padStart(12, '0')}`

const summary = (
  index: number,
  state:
    | 'active'
    | 'queued'
    | 'blocked'
    | 'awaiting_approval'
    | 'ambiguous'
    | 'awaiting_reconciliation'
    | 'runner_lost'
    | 'idle' = 'idle',
) => ({
  active_turn_count: state === 'active' ? '1' : '0',
  archived: false,
  current_turn_id: state === 'idle' ? null : sessionId(20_000 + index),
  judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
  last_activity: { kind: 'turn' as const, unix_milliseconds: String(1_787_400_000_000 - index) },
  queued_turn_count: state === 'queued' ? '2' : '0',
  session_id: sessionId(index),
  state,
  title_summary: `Session ${index.toString().padStart(4, '0')}`,
  title_truncated: false,
})

const catalogPage = (start: number, search: string | null) => {
  const summaries = search
    ? [summary(900, 'blocked')]
    : Array.from({ length: 128 }, (_, offset) => summary(start + offset))
  const last = summaries.at(-1)
  return {
    continuation:
      search || !last
        ? null
        : {
            kind: 'last_activity' as const,
            session_id: last.session_id,
            unix_microseconds: `${BigInt(last.last_activity.unix_milliseconds) * 1000n}`,
          },
    cursor: '42',
    sort: 'last_activity_descending' as const,
    summaries,
    total: '1000',
  }
}

const snapshot = (
  id: string,
  activeState:
    | { kind: 'running'; model_call_id: string }
    | { kind: 'awaiting_tool_approval'; tool_request_id: string },
  reconciliation: null | { kind: 'model_call'; model_call_id: string; turn_id: string } = null,
) => ({
  active: { state: activeState, turn_id: sessionId(30_001) },
  observed_through: '42',
  queued_turn_count: '1',
  queued_turn_ids: [sessionId(30_002)],
  reconciliation,
  runner: {
    connection_health: 'connected',
    placement_revision: '3',
    runner_id: sessionId(30_003),
    state: 'pinned',
  },
  session_id: id,
})

const ndjson = (...records: readonly unknown[]) =>
  `${records.map((record) => JSON.stringify(record)).join('\n')}\n`

const installBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: webContractBootstrapFixture }))

const installTimeline = (page: Page, id: string) =>
  page.route('**/api/sessions/**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    const requestedSession = pathname.split('/')[3] ?? id
    return pathname.endsWith('/timeline')
      ? route.fulfill({
          json: {
            session_id: requestedSession,
            items: [
              {
                address: { event_sequence: '41' },
                kind: 'turn_activated',
                projected_structured_bytes: 78,
              },
              {
                address: { event_sequence: '42' },
                kind: 'model_call_transition',
                projected_structured_bytes: 85,
              },
            ],
            projected_structured_bytes: 163,
            continuation_before: { event_sequence: '41' },
            continuation_after: null,
          },
        })
      : pathname.endsWith('/live')
        ? route.fulfill({
            json: snapshot(requestedSession, {
              kind: 'running',
              model_call_id: sessionId(40_001),
            }),
          })
        : route.fulfill({
            json: {
              session_id: requestedSession,
              sizes: {
                item_count: '1000000',
                projected_text_bytes: '48000000',
                projected_structured_bytes: '96000000',
                referenced_blob_count: '24000',
                referenced_blob_bytes: '96000000000',
              },
              first_address: { event_sequence: '1' },
              latest_address: { event_sequence: '42' },
              work: { active_turn_count: '1', queued_turn_count: '1' },
              observed_through: '42',
            },
          })
  })

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

interface Deferred {
  promise: Promise<void>
  resolve: () => void
}

const deferred = (resolved = false): Deferred => {
  let resolve: () => void = () => undefined
  const promise = new Promise<void>((done) => {
    resolve = done
  })
  if (resolved) resolve()
  return { promise, resolve }
}

test('pages and searches one thousand sessions while retaining a virtualized client window', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  const catalogRequests: string[] = []
  let fleetFollowRequests = 0
  const fleetFollowGate = deferred()
  await installBootstrap(page)
  await page.route('**/api/attention/follow', async (route) => {
    fleetFollowRequests += 1
    await fleetFollowGate.promise
    return route.fulfill({
      contentType: 'application/x-ndjson',
      body: ndjson({ kind: 'snapshot', snapshot: catalogPage(0, null) }),
    })
  })
  await page.route('**/api/sessions**', (route) => {
    const url = new URL(route.request().url())
    const search = url.searchParams.get('search')
    const after = url.searchParams.get('after_session_id')
    const start = after ? Number.parseInt(after.slice(-12), 16) + 1 : 0
    catalogRequests.push(url.toString())
    return route.fulfill({ json: catalogPage(start, search) })
  })
  await page.goto('/sessions')

  await expect(page.getByText('128 retained')).toBeVisible()
  await page.getByRole('button', { name: 'Load more' }).click()
  await expect(page.getByText('256 retained')).toBeVisible()
  await page.getByRole('button', { name: 'Load more' }).click()
  await expect(page.getByText('384 retained')).toBeVisible()
  await page.getByRole('button', { name: 'Load more' }).click()
  await expect(page.getByText('512 retained')).toBeVisible()
  expect(await page.getByRole('option').count()).toBeLessThan(40)
  await page.keyboard.press('/')
  await expect(page.getByRole('searchbox')).toBeFocused()
  await page.getByRole('searchbox').fill('Session 0900')
  await page.getByRole('searchbox').press('Enter')
  await expect(page.getByText(summary(900, 'blocked').title_summary)).toBeVisible()
  expect(catalogRequests.at(-1)).toContain('search=Session+0900')
  expect(fleetFollowRequests).toBe(1)
  fleetFollowGate.resolve()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('opens a catalog row and rapidly switches watched sessions from the keyboard', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  const first = summary(701, 'active')
  const second = summary(702, 'queued')
  const third = summary(703, 'idle')
  const followRequests: string[] = []
  await installBootstrap(page)
  await installTimeline(page, first.session_id)
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: ndjson({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '42',
          sort: 'last_activity_descending',
          summaries: [first, second, third],
          total: '3',
        },
      }),
    }),
  )
  await page.route('**/api/sessions**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    const requestedSession = pathname.split('/')[3] ?? first.session_id
    return pathname === '/api/sessions'
      ? route.fulfill({
          json: {
            continuation: null,
            cursor: '42',
            sort: 'last_activity_descending',
            summaries: [first, second, third],
            total: '3',
          },
        })
      : pathname.endsWith('/follow')
        ? (() => {
            followRequests.push(pathname)
            return route.fulfill({
              contentType: 'application/x-ndjson',
              body: ndjson({
                kind: 'snapshot',
                snapshot: snapshot(requestedSession, {
                  kind: 'running',
                  model_call_id: sessionId(40_001),
                }),
              }),
            })
          })()
        : route.fallback()
  })
  await page.goto('/sessions')

  const catalog = page.getByRole('listbox', { name: 'Sessions' })
  await catalog.focus()
  await page.keyboard.press('Alt+j')
  await page.keyboard.press('Alt+k')
  await catalog.press('Enter')
  await expect(page.getByRole('heading', { name: first.title_summary })).toBeVisible()
  expect(followRequests).toContain(`/api/sessions/${first.session_id}/follow`)
  await page.keyboard.press(']')
  await expect(page.getByRole('heading', { name: second.title_summary })).toBeVisible()
  await page.keyboard.press('[')
  await expect(page.getByRole('heading', { name: first.title_summary })).toBeVisible()
  expect(followRequests).not.toContain(`/api/sessions/${third.session_id}/follow`)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('resynchronizes provider and durable updates through approval and reconciliation parks', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  const watchedSession = sessionId(991)
  const watchedSummary = summary(991, 'active')
  const gates = [deferred(true), deferred(), deferred(), deferred()]
  const bodies = [
    ndjson(
      {
        kind: 'snapshot',
        snapshot: snapshot(watchedSession, {
          kind: 'running',
          model_call_id: sessionId(40_001),
        }),
      },
      {
        content: 'A non-authoritative provider draft',
        kind: 'provider_text_delta',
        model_call_id: sessionId(40_001),
        part_index: 0,
        turn_id: sessionId(30_001),
      },
    ),
    ndjson(
      {
        kind: 'snapshot',
        snapshot: snapshot(watchedSession, {
          kind: 'awaiting_tool_approval',
          tool_request_id: sessionId(50_001),
        }),
      },
      {
        address: { event_sequence: '43' },
        cursor: '43',
        event_kind: 'turn_reconciliation_required',
        kind: 'durable',
      },
      {
        content: 'discard me on lag',
        kind: 'provider_text_delta',
        model_call_id: sessionId(40_001),
        part_index: 0,
        turn_id: sessionId(30_001),
      },
      { cursor: '43', kind: 'resync_required' },
    ),
    ndjson({
      kind: 'snapshot',
      snapshot: snapshot(
        watchedSession,
        { kind: 'running', model_call_id: sessionId(40_002) },
        { kind: 'model_call', model_call_id: sessionId(40_001), turn_id: sessionId(30_001) },
      ),
    }),
    ndjson(
      {
        kind: 'snapshot',
        snapshot: snapshot(watchedSession, {
          kind: 'running',
          model_call_id: sessionId(40_003),
        }),
      },
      {
        address: { event_sequence: '44' },
        cursor: '44',
        event_kind: 'turn_completed',
        kind: 'durable',
      },
    ),
  ]
  const liveSnapshots = [
    snapshot(watchedSession, { kind: 'running', model_call_id: sessionId(40_001) }),
    snapshot(watchedSession, {
      kind: 'awaiting_tool_approval',
      tool_request_id: sessionId(50_001),
    }),
    snapshot(
      watchedSession,
      { kind: 'running', model_call_id: sessionId(40_002) },
      { kind: 'model_call', model_call_id: sessionId(40_001), turn_id: sessionId(30_001) },
    ),
    snapshot(watchedSession, { kind: 'running', model_call_id: sessionId(40_003) }),
  ]
  let followRequest = 0
  await installBootstrap(page)
  await installTimeline(page, watchedSession)
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: ndjson({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '42',
          sort: 'last_activity_descending',
          summaries: [watchedSummary],
          total: '1',
        },
      }),
    }),
  )
  await page.route('**/api/sessions**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    return pathname === '/api/sessions'
      ? route.fulfill({
          json: {
            continuation: null,
            cursor: '42',
            sort: 'last_activity_descending',
            summaries: [watchedSummary],
            total: '1',
          },
        })
      : pathname.endsWith('/follow')
        ? gates[followRequest]?.promise.then(() => {
            const body = bodies[followRequest] ?? bodies.at(-1) ?? ''
            followRequest += 1
            return route.fulfill({ contentType: 'application/x-ndjson', body })
          })
        : pathname.endsWith('/live')
          ? route.fulfill({
              json: liveSnapshots[Math.max(0, followRequest - 1)] ?? liveSnapshots.at(-1),
            })
          : route.fallback()
  })
  await page.goto('/sessions')
  await page.getByRole('option', { name: new RegExp(watchedSummary.title_summary) }).click()

  await expect(page.getByText('resynchronizing')).toBeVisible()
  await expect(page.getByText('A non-authoritative provider draft')).toHaveCount(0)
  gates[1]?.resolve()
  await expect(page.getByText('Turn · Awaiting approval')).toBeVisible()
  await expect(page.getByText('discard me on lag')).toHaveCount(0)
  gates[2]?.resolve()
  await expect(page.getByText(/Awaiting reconciliation · model call/)).toBeVisible()
  gates[3]?.resolve()
  await expect(page.getByText('Running')).toBeVisible()
  await expect(page.getByText('resynchronizing')).toBeVisible()
  await expect(page.getByRole('option', { name: /44 turn completed/ })).toHaveCount(0)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

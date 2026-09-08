import type { WebSessionTimelineDetail } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'

const sessionId = '00000000-0000-0000-0000-000000000991'
const turnId = '00000000-0000-0000-0000-000000000992'
const initialMessage = 'Check the session and explain the next step.'
const assistantMessage = 'The session is ready. I can continue from the recorded conversation.'
const excerpt = (text: string) => ({
  text,
  offset_bytes: '0',
  total_bytes: String(new TextEncoder().encode(text).length),
  continuation: null,
})

async function sessionApi(page: Page, busy = false, selectedSessionId = sessionId) {
  const state = {
    active: busy,
    grown: false,
    observed: false,
    historyReads: [] as string[],
    textReads: [] as string[],
    submissions: [] as Array<{ command_id: string; message: string }>,
  }
  let releaseFollow = () => {}
  const followReady = new Promise<void>((resolve) => {
    releaseFollow = resolve
  })
  const snapshot = () => ({
    session_id: selectedSessionId,
    observed_through: state.grown || state.observed ? '44' : '43',
    active: state.active
      ? { turn_id: turnId, state: { kind: 'running', model_call_id: null } }
      : null,
    queued_turn_count: '0',
    queued_turn_ids: [],
    reconciliation: null,
    runner: null,
  })
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention', (route) =>
    route.fulfill({ json: { cursor: '0', summaries: [], continuation_after_session_id: null } }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: { cursor: '0', summaries: [], continuation_after_session_id: null } })}\n`,
    }),
  )
  await page.route(`**/api/sessions/${selectedSessionId}**`, async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/input')) {
      state.submissions.push(route.request().postDataJSON())
      return route.fulfill({ status: 204 })
    }
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: snapshot() })
    if (url.pathname.endsWith('/follow')) {
      const initial = snapshot()
      await followReady
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: initial })}\n${JSON.stringify({ kind: 'durable', cursor: '44', address: { event_sequence: '44' }, event_kind: 'model_call_transition' })}\n`,
      })
    }
    if (url.pathname.endsWith('/timeline-detail')) {
      state.textReads.push(url.searchParams.get('first') ?? '')
      const items: WebSessionTimelineDetail[] = [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          projected_body_bytes: 128 + initialMessage.length,
          body: {
            type: 'user_input',
            turn_id: turnId,
            text: excerpt(initialMessage),
            attachments: [],
          },
        },
      ]
      if (state.grown)
        items.push({
          address: { event_sequence: '44' },
          kind: 'model_call_transition',
          projected_body_bytes: 128 + assistantMessage.length,
          body: {
            type: 'model_call',
            turn_id: turnId,
            model_call_id: turnId,
            model_identity_id: turnId,
            request_context_items: '1',
            response: excerpt(assistantMessage),
            state: { type: 'terminal', disposition: 'completed' },
            usage: {},
          },
        })
      const selected = items.filter(
        (item) =>
          BigInt(item.address.event_sequence) >= BigInt(url.searchParams.get('first') ?? '0'),
      )
      return route.fulfill({
        json: {
          session_id: selectedSessionId,
          projected_body_bytes: selected.reduce((sum, item) => sum + item.projected_body_bytes, 0),
          items: selected,
          continuation: null,
        },
      })
    }
    const latest = state.grown ? '44' : '43'
    if (url.pathname.endsWith('/timeline')) {
      state.historyReads.push(url.searchParams.get('anchor') ?? '')
      return route.fulfill({
        json: {
          session_id: selectedSessionId,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '43' },
              kind: 'turn_completed',
              projected_structured_bytes: 78,
            },
            ...(state.grown
              ? [
                  {
                    address: { event_sequence: '44' },
                    kind: 'model_call_transition',
                    projected_structured_bytes: 85,
                  },
                ]
              : []),
          ].filter(
            (item) =>
              url.searchParams.get('anchor') !== 'after' ||
              BigInt(item.address.event_sequence) > BigInt(url.searchParams.get('address') ?? '0'),
          ),
          projected_structured_bytes:
            url.searchParams.get('anchor') === 'after' ? 85 : state.grown ? 241 : 156,
          continuation_before:
            url.searchParams.get('anchor') === 'after' ? { event_sequence: '44' } : null,
          continuation_after: null,
        },
      })
    }
    return route.fulfill({
      json: {
        session_id: selectedSessionId,
        sizes: {
          item_count: state.grown ? '3' : '2',
          projected_text_bytes: String(
            initialMessage.length + (state.grown ? assistantMessage.length : 0),
          ),
          projected_structured_bytes: state.grown ? '241' : '156',
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: '41' },
        latest_address: { event_sequence: latest },
        observed_through: state.observed ? '44' : latest,
        work: { active_turn_count: state.active ? '1' : '0', queued_turn_count: '0' },
      },
    })
  })
  return {
    state,
    advanceObservation: () => {
      state.observed = true
      releaseFollow()
    },
    grow: () => {
      state.grown = true
      releaseFollow()
    },
  }
}

async function openSession(page: Page) {
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
}

test('reads durable transcript growth and sends a message by keyboard', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  api.grow()
  await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
  expect(api.state.historyReads).toEqual(['latest', 'after'])
  expect(api.state.textReads).toEqual(['41', '44'])
  await page
    .getByRole('textbox', { name: 'Message to session' })
    .fill('Continue with the next step.')
  await page.getByRole('button', { name: 'Send message', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('status').filter({ hasText: 'Message accepted by the daemon.' }),
  ).toBeVisible()
  expect(api.state.submissions).toHaveLength(1)
  expect(api.state.submissions[0]?.message).toBe('Continue with the next step.')
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toHaveValue('')
})

test('follows new active work after restoring an inactive session position', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  await page.getByRole('option', { name: /41 input accepted/ }).click()
  await expect
    .poll(() =>
      page.evaluate((id) => {
        const stored = JSON.parse(localStorage.getItem('signalbox.web.preferences.v1') ?? '{}')
        return stored.lastLogicalPositions?.[id]
      }, sessionId),
    )
    .toBe('41')
  await page.reload()
  await expect(page.getByText('Inactive · restored logical position')).toBeVisible()
  expect(api.state.historyReads.at(-1)).toBe('around')
  api.state.active = true
  api.grow()
  await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  expect(api.state.historyReads).toEqual(['latest', 'around', 'after'])
})

test('retries an unconfirmed acceptance with the same command and text', async ({ page }) => {
  const api = await sessionApi(page)
  const attempts: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/input`, (route) => {
    attempts.push(route.request().postDataJSON())
    return attempts.length === 1 ? route.abort() : route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('textbox', { name: 'Message to session' }).fill('Preserve this message.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect(
    page.getByText('Acceptance is unconfirmed. Retry sends the same command and message.'),
  ).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toHaveAttribute(
    'readonly',
    '',
  )
  await expect(page.getByRole('button', { name: 'Discard retained command' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toHaveCount(0)
  await page
    .getByRole('link', { name: 'Settings Local workspace preferences', exact: true })
    .click()
  await expect(page.getByRole('form', { name: 'Message composer' })).toHaveCount(0)
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toHaveValue(
    'Preserve this message.',
  )
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toHaveAttribute(
    'readonly',
    '',
  )
  await page.route(`**/api/sessions/${sessionId}/timeline?**`, (route) =>
    route.fulfill({ status: 503, body: 'Timeline temporarily unavailable' }),
  )
  api.grow()
  await expect(page.getByText('Live updates unavailable.')).toBeVisible()
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByText('Message accepted by the daemon.')).toBeVisible()
  expect(attempts).toHaveLength(2)
  expect(attempts[1]).toEqual(attempts[0])
  api.grow()
})

test('retains a command whose response is lost while its composer is unmounted', async ({
  page,
}) => {
  const api = await sessionApi(page)
  const attempts: unknown[] = []
  let loseResponse = () => {}
  const responsePending = new Promise<void>((resolve) => {
    loseResponse = resolve
  })
  await page.route(`**/api/sessions/${sessionId}/input`, async (route) => {
    attempts.push(route.request().postDataJSON())
    if (attempts.length === 1) {
      await responsePending
      return route.abort()
    }
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page
    .getByRole('textbox', { name: 'Message to session' })
    .fill('Keep the in-flight identity.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect.poll(() => attempts.length).toBe(1)
  await page
    .getByRole('link', { name: 'Settings Local workspace preferences', exact: true })
    .click()
  await expect(page.getByRole('form', { name: 'Message composer' })).toHaveCount(0)
  loseResponse()
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toHaveValue(
    'Keep the in-flight identity.',
  )
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByText('Message accepted by the daemon.')).toBeVisible()
  expect(attempts).toHaveLength(2)
  expect(attempts[1]).toEqual(attempts[0])
  api.grow()
})

test('reports the daemon rejection reason and keeps the draft editable', async ({ page }) => {
  const api = await sessionApi(page)
  await page.route(`**/api/sessions/${sessionId}/input`, (route) =>
    route.fulfill({
      status: 409,
      json: {
        error: {
          kind: 'application',
          code: 'active_turn_present',
          message: 'input cannot start a turn while another turn is active',
        },
      },
    }),
  )
  await openSession(page)
  await page.getByRole('textbox', { name: 'Message to session' }).fill('Keep the rejected draft.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect(
    page.getByText('Rejected: input cannot start a turn while another turn is active'),
  ).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toBeEditable()
  api.grow()
})

test('shows when an active turn prevents starting another turn', async ({ page }) => {
  const api = await sessionApi(page, true)
  await openSession(page)
  await expect(page.getByText('Input unavailable: running')).toBeVisible()
  await page.getByRole('textbox', { name: 'Message to session' }).fill('A draft for later.')
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
  expect(api.state.submissions).toHaveLength(0)
  api.grow()
})

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`read and send workspace at ${viewport.name} size`, async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'Chromium owns product visual goldens')
    await page.setViewportSize(viewport)
    const api = await sessionApi(page)
    await openSession(page)
    api.grow()
    await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
    await expect(page.getByText('Live updates unavailable.')).toBeVisible()
    await page
      .getByRole('textbox', { name: 'Message to session' })
      .fill('Continue with the next step.')
    await expect.soft(page).toHaveScreenshot(`session-read-send-${viewport.name}.png`, {
      fullPage: true,
    })
  })
}

test('keeps header-only history when transcript detail is not advertised', async ({ page }) => {
  const api = await sessionApi(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_session_timeline_detail: false },
      },
    }),
  )
  const detailRequests: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/timeline-detail')) detailRequests.push(request.url())
  })
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Transcript text' })).toHaveCount(0)
  await expect(page.getByRole('textbox', { name: 'Message to session' })).toBeVisible()
  api.grow()
  await expect(page.getByRole('option')).toHaveCount(3)
  expect(detailRequests).toEqual([])
})

test('refuses new session input at the retained-command limit while allowing exact retries', async ({
  page,
}) => {
  const ids = [
    sessionId,
    '00000000-0000-0000-0000-000000000993',
    '00000000-0000-0000-0000-000000000994',
    '00000000-0000-0000-0000-000000000995',
    '00000000-0000-0000-0000-000000000996',
  ] as const
  const apis = []
  const attempts: Array<{ command_id: string; message: string }> = []
  for (const id of ids) apis.push(await sessionApi(page, false, id))
  await page.route('**/api/sessions/*/input', (route) => {
    attempts.push(route.request().postDataJSON())
    return route.abort()
  })
  await openSession(page)
  const open = async (id: string) => {
    await page.getByRole('textbox', { name: 'Exact session ID' }).fill(id)
    await page.getByRole('button', { name: 'Open workspace', exact: true }).click()
  }
  for (const id of ids.slice(0, 4)) {
    await open(id)
    await page.getByRole('textbox', { name: 'Message to session' }).fill('Retain this command.')
    await page.getByRole('button', { name: 'Send message', exact: true }).click()
    await expect(
      page.getByText('Acceptance is unconfirmed. Retry sends the same command and message.'),
    ).toBeVisible()
  }
  const last = ids[4]
  await open(last)
  await page.getByRole('textbox', { name: 'Message to session' }).fill('Wait for capacity.')
  await expect(
    page.getByText(
      'Pending-message limit reached. Retry a retained message before sending to another session.',
    ),
  ).toBeVisible()
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
  expect(attempts).toHaveLength(4)
  await page.route(`**/api/sessions/${sessionId}/input`, (route) => {
    attempts.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await open(sessionId)
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByText('Message accepted by the daemon.')).toBeVisible()
  expect(attempts[4]).toEqual(attempts[0])
  await open(last)
  await page
    .getByRole('textbox', { name: 'Message to session' })
    .fill('Capacity is available again.')
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeEnabled()
  for (const api of apis) api.grow()
})

test('times out an unanswered send and retries its retained identity', async ({ page }) => {
  const api = await sessionApi(page)
  await page.clock.install()
  const attempts: Array<{ command_id: string; message: string }> = []
  let release = () => {}
  const stalled = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route(`**/api/sessions/${sessionId}/input`, async (route) => {
    attempts.push(route.request().postDataJSON())
    if (attempts.length === 1) {
      await stalled
      return route.abort()
    }
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  const draft = page.getByRole('textbox', { name: 'Message to session' })
  await expect(draft).toHaveAttribute('maxlength', '65536')
  await draft.fill('Keep the deadline identity.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect.poll(() => attempts.length).toBe(1)
  await page.clock.fastForward(30_001)
  await expect(
    page.getByText('Acceptance is unconfirmed. Retry sends the same command and message.'),
  ).toBeVisible()
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByText('Message accepted by the daemon.')).toBeVisible()
  expect(attempts[1]).toEqual(attempts[0])
  release()
  api.grow()
})

test('uses smaller advertised transcript limits in the workspace', async ({ page }) => {
  const api = await sessionApi(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: {
          ...bootstrapFixture.limits,
          max_timeline_detail_items: 1,
          max_timeline_detail_bytes: 1024,
        },
      },
    }),
  )
  const requests: URL[] = []
  page.on('request', (request) => {
    if (request.url().includes('/timeline-detail')) requests.push(new URL(request.url()))
  })
  await openSession(page)
  expect(requests.length).toBeGreaterThan(0)
  expect(
    requests.every(
      (url) =>
        url.searchParams.get('max_items') === '1' && url.searchParams.get('max_bytes') === '1024',
    ),
  ).toBe(true)
  api.advanceObservation()
})

test('resets text pagination when only the window observation changes', async ({ page }) => {
  const api = await sessionApi(page)
  const cursors: Array<string | null> = []
  await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) => {
    const cursor = new URL(route.request().url()).searchParams.get('cursor_address')
    cursors.push(cursor)
    const item: WebSessionTimelineDetail =
      cursor === null
        ? {
            address: { event_sequence: '41' },
            kind: 'input_accepted',
            projected_body_bytes: 128 + initialMessage.length,
            body: {
              type: 'user_input',
              turn_id: turnId,
              text: excerpt(initialMessage),
              attachments: [],
            },
          }
        : {
            address: { event_sequence: '43' },
            kind: 'turn_completed',
            projected_body_bytes: 128,
            body: {
              type: 'turn_lifecycle',
              turn_id: turnId,
              lifecycle: 'terminalized',
              cause_code: 'completed',
            },
          }
    return route.fulfill({
      json: {
        session_id: sessionId,
        items: [item],
        projected_body_bytes: item.projected_body_bytes,
        continuation:
          cursor === null ? { type: 'more_at', address: { event_sequence: '43' } } : null,
      },
    })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Next text page' }).click()
  await expect(page.getByRole('button', { name: 'First text page' })).toBeVisible()
  await expect(page.getByText('Reading transcript text…', { exact: true })).toHaveCount(0)
  await expect(
    page.getByRole('region', { name: 'Transcript text' }).getByRole('alert'),
  ).toHaveCount(0)
  expect(cursors).toEqual([null, '43'])
  api.advanceObservation()
  await expect(page.getByText('Live updates unavailable.')).toBeVisible()
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'First text page' })).toHaveCount(0)
  expect(cursors.slice(2).length).toBeGreaterThan(0)
  expect(cursors.slice(2).every((cursor) => cursor === null)).toBe(true)
})

test('keeps earlier text reachable after live appending evicts a text-page entry', async ({
  page,
}) => {
  const api = await sessionApi(page)
  const latest = () => (api.state.grown ? 9 : 8)
  await page.route(`**/api/sessions/${sessionId}`, (route) =>
    route.fulfill({
      json: {
        session_id: sessionId,
        sizes: {
          item_count: String(latest()),
          projected_text_bytes: String(latest() * 7),
          projected_structured_bytes: String(latest() * 78),
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: '1' },
        latest_address: { event_sequence: String(latest()) },
        observed_through: api.state.grown ? '44' : '43',
        work: { active_turn_count: '0', queued_turn_count: '0' },
      },
    }),
  )
  await page.route(`**/api/sessions/${sessionId}/timeline?**`, (route) => {
    const url = new URL(route.request().url())
    const first =
      url.searchParams.get('anchor') === 'after' ? Number(url.searchParams.get('address')) + 1 : 1
    const items = Array.from({ length: latest() - first + 1 }, (_, index) => ({
      address: { event_sequence: String(first + index) },
      kind: 'input_accepted',
      projected_structured_bytes: 78,
    }))
    return route.fulfill({
      json: {
        session_id: sessionId,
        items,
        projected_structured_bytes: items.length * 78,
        continuation_before: first > 1 ? { event_sequence: String(first) } : null,
        continuation_after: null,
      },
    })
  })
  const reads: string[] = []
  await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) => {
    const url = new URL(route.request().url())
    const first = Number(url.searchParams.get('cursor_address') ?? url.searchParams.get('first'))
    const through = Number(url.searchParams.get('through'))
    reads.push(String(first))
    const end = Math.min(first + 7, through)
    const items = Array.from({ length: end - first + 1 }, (_, index) => ({
      address: { event_sequence: String(first + index) },
      kind: 'input_accepted',
      projected_body_bytes: 135,
      body: {
        type: 'user_input',
        turn_id: turnId,
        text: excerpt(`Entry ${first + index}`),
        attachments: [],
      },
    }))
    return route.fulfill({
      json: {
        session_id: sessionId,
        items,
        projected_body_bytes: items.length * 135,
        continuation:
          end < through ? { type: 'more_at', address: { event_sequence: String(end + 1) } } : null,
      },
    })
  })
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await expect(page.getByText('Entry 1', { exact: true })).toBeVisible()
  api.grow()
  await expect(page.getByText('Entry 9', { exact: true })).toBeVisible()
  await expect(page.getByText('Entry 1', { exact: true })).toHaveCount(0)
  expect(reads).toEqual(['1', '9'])
  await page.getByRole('button', { name: 'First text page', exact: true }).click()
  await expect(page.getByText('Entry 1', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Next text page', exact: true }).click()
  await expect(page.getByText('Entry 9', { exact: true })).toBeVisible()
  expect(reads).toEqual(['1', '9', '1', '9'])
})

for (const growth of [false, true]) {
  test(`keeps a paginated first page through ${growth ? 'tail-growing' : 'observation-only'} resync`, async ({
    page,
  }) => {
    const api = await sessionApi(page)
    let resynchronize = () => {}
    const ready = new Promise<void>((resolve) => {
      resynchronize = resolve
    })
    const live = (observed: string) => ({
      session_id: sessionId,
      observed_through: observed,
      active: null,
      queued_turn_count: '0',
      queued_turn_ids: [],
      reconciliation: null,
      runner: null,
    })
    let follows = 0
    await page.route(`**/api/sessions/${sessionId}/follow`, async (route) => {
      const initial = follows++ === 0
      if (initial) await ready
      const events = [
        { kind: 'snapshot', snapshot: live(initial ? '43' : '44') },
        ...(initial ? [{ kind: 'resync_required', cursor: '44' }] : []),
      ]
      await route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${events.map((event) => JSON.stringify(event)).join('\n')}\n`,
      })
    })
    let textReads = 0
    await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) => {
      textReads++
      if (textReads > 1)
        return route.fulfill({ status: 503, body: 'Historical reread unavailable' })
      const item = {
        address: { event_sequence: '41' },
        kind: 'input_accepted',
        projected_body_bytes: 128 + initialMessage.length,
        body: {
          type: 'user_input',
          turn_id: turnId,
          text: excerpt(initialMessage),
          attachments: [],
        },
      }
      return route.fulfill({
        json: {
          session_id: sessionId,
          items: [item],
          projected_body_bytes: item.projected_body_bytes,
          continuation: { type: 'more_at', address: { event_sequence: '43' } },
        },
      })
    })
    await openSession(page)
    await expect(page.getByRole('button', { name: 'Next text page', exact: true })).toBeVisible()
    if (growth) api.grow()
    else api.advanceObservation()
    resynchronize()
    await expect(
      page
        .getByRole('region', { name: sessionId, exact: true })
        .getByRole('definition')
        .filter({ hasText: /^44$/ }),
    ).toBeVisible()
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Next text page', exact: true })).toBeVisible()
    if (growth) expect(api.state.historyReads).toContain('after')
    expect(textReads).toBe(1)
  })
}

import { BROWSER_PREFERENCES_KEY, createDefaultBrowserPreferences } from '../src/preferences'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, test } from './fontTest'
import {
  assistantMessage,
  excerpt,
  initialMessage,
  openSession,
  openSessionFromCatalog,
  sessionApi,
  sessionId,
  turnId,
  type WebRepositoryWatchProvenance,
} from './session-fixture'

test('reads durable transcript growth and sends a message by keyboard', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  api.grow()
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
  expect(api.state.historyReads).toContain('after')
  expect(api.state.textReads).toContain('44')
  await page.getByRole('textbox', { name: 'Message' }).fill('Continue with the next step.')
  await page.getByRole('button', { name: 'Send message', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
  expect(api.state.submissions).toHaveLength(1)
  expect(api.state.submissions[0]?.message).toBe('Continue with the next step.')
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveValue('')
})

test('follows new active work after restoring an inactive session position', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await page.getByRole('row', { name: /41 Message accepted/ }).click()
  await expect
    .poll(() =>
      page.evaluate((id) => {
        const stored = JSON.parse(localStorage.getItem('signalbox.web.preferences.v1') ?? '{}')
        return stored.lastLogicalPositions?.[id]
      }, sessionId),
    )
    .toBe('41')
  await page.reload()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Inactive$/ })).toBeVisible()
  expect(api.state.historyReads).toContain('around')
  api.state.active = true
  api.grow()
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
  await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
  expect(api.state.historyReads).toContain('after')
})

test('retries an unconfirmed acceptance with the same command and text', async ({ page }) => {
  const api = await sessionApi(page)
  const attempts: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/input`, (route) => {
    attempts.push(route.request().postDataJSON())
    return attempts.length === 1 ? route.abort() : route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('textbox', { name: 'Message' }).fill('Preserve this message.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect(page.getByText('Delivery unconfirmed')).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveAttribute('readonly', '')
  await expect(page.getByRole('button', { name: 'Discard retained command' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toHaveCount(0)
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await expect(page.getByRole('form', { name: 'Message composer' })).toHaveCount(0)
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveValue('Preserve this message.')
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveAttribute('readonly', '')
  await page.route(`**/api/sessions/${sessionId}/timeline?**`, (route) =>
    route.fulfill({ status: 503, body: 'Timeline temporarily unavailable' }),
  )
  api.grow()
  await expect(page.getByText('Live updates unavailable.')).toBeVisible()
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
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
  await page.getByRole('textbox', { name: 'Message' }).fill('Keep the in-flight identity.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect.poll(() => attempts.length).toBe(1)
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await expect(page.getByRole('form', { name: 'Message composer' })).toHaveCount(0)
  loseResponse()
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Message' })).toHaveValue(
    'Keep the in-flight identity.',
  )
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
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
  await page.getByRole('textbox', { name: 'Message' }).fill('Keep the rejected draft.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect(
    page.getByText('Message rejected: input cannot start a turn while another turn is active'),
  ).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Message' })).toBeEditable()
  api.grow()
})

test('shows when an active turn prevents starting another turn', async ({ page }) => {
  const api = await sessionApi(page, true)
  await openSession(page)
  await expect(page.getByText('Wait for the current turn to finish · Running')).toBeVisible()
  await page.getByRole('textbox', { name: 'Message' }).fill('A draft for later.')
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
    await page.getByRole('radio', { name: 'All details', exact: true }).check()
    await expect(page.getByText(assistantMessage, { exact: true })).toBeVisible()
    await expect(page.getByText('Live updates unavailable.')).toBeVisible()
    await page.getByRole('textbox', { name: 'Message' }).fill('Continue with the next step.')
    await expect.soft(page).toHaveScreenshot(`session-read-send-${viewport.name}.png`, {
      fullPage: true,
    })
  })
}

test('keeps header-only history when transcript detail is not advertised', async ({ page }) => {
  await page.addInitScript(
    ({ key, preferences }) => {
      localStorage.setItem(key, JSON.stringify(preferences))
    },
    {
      key: BROWSER_PREFERENCES_KEY,
      preferences: { ...createDefaultBrowserPreferences(), detail: 'full' },
    },
  )
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
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Transcript text' })).toHaveCount(0)
  await expect(page.getByRole('textbox', { name: 'Message' })).toBeVisible()
  api.grow()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('row')).toHaveCount(3)
  await page.getByRole('row').first().press('Enter')
  await expect(page.getByText('Timeline detail unavailable.', { exact: true })).toBeVisible()
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
    await openSessionFromCatalog(page, id)
  }
  for (const id of ids.slice(0, 4)) {
    await open(id)
    await page.getByRole('textbox', { name: 'Message' }).fill('Retain this command.')
    await page.getByRole('button', { name: 'Send message', exact: true }).click()
    await expect(page.getByText('Delivery unconfirmed')).toBeVisible()
  }
  const last = ids[4]
  await open(last)
  await page.getByRole('textbox', { name: 'Message' }).fill('Wait for capacity.')
  await expect(page.getByText('Too many unconfirmed messages')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
  expect(attempts).toHaveLength(4)
  await page.route(`**/api/sessions/${sessionId}/input`, (route) => {
    attempts.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await open(sessionId)
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
  expect(attempts[4]).toEqual(attempts[0])
  await open(last)
  await page.getByRole('textbox', { name: 'Message' }).fill('Capacity is available again.')
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
  const draft = page.getByRole('textbox', { name: 'Message' })
  await expect(draft).toHaveAttribute('maxlength', '65536')
  await draft.fill('Keep the deadline identity.')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect.poll(() => attempts.length).toBe(1)
  await page.clock.fastForward(30_001)
  await expect(page.getByText('Delivery unconfirmed')).toBeVisible()
  await page.getByRole('button', { name: 'Retry message' }).click()
  await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
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
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await page.getByRole('button', { name: 'First', exact: true }).click()
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
  expect(requests.length).toBeGreaterThan(0)
  expect(
    requests.every(
      (url) =>
        url.searchParams.get('max_items') === '1' && url.searchParams.get('max_bytes') === '1024',
    ),
  ).toBe(true)
  api.advanceObservation()
})

test('refreshes the bounded transcript when only the observation changes', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  const reads = api.state.textReads.length
  api.advanceObservation()
  await expect(page.getByText('Live updates unavailable.')).toBeVisible()
  await expect.poll(() => api.state.textReads.length).toBeGreaterThan(reads)
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
  await expect(
    page.getByRole('region', { name: 'Transcript text' }).getByRole('alert'),
  ).toHaveCount(0)
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toBeInViewport()
})

test('keeps earlier text reachable after live growth', async ({ page }) => {
  const api = await sessionApi(page)
  const latest = () => (api.state.grown ? 9 : 8)
  await page.route(`**/api/sessions/${sessionId}`, (route) =>
    route.fulfill({
      json: {
        session_id: sessionId,
        supervision: null,
        repository_watch: null,
        workspace_root_kind: null,
        title_summary: null,
        last_activity: { kind: 'session', unix_microseconds: '1' },
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
    const anchor = url.searchParams.get('anchor')
    const address = Number(url.searchParams.get('address'))
    const end = anchor === 'before' ? address - 1 : latest()
    const first =
      anchor === 'after'
        ? address + 1
        : Math.max(1, end - Number(url.searchParams.get('max_items') ?? 8) + 1)
    const items = Array.from({ length: end - first + 1 }, (_, index) => ({
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
        continuation_after: end < latest() ? { event_sequence: String(end) } : null,
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
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.evaluate((element) => {
    element.scrollTop = 0
  })
  await transcript.hover()
  await page.mouse.wheel(0, -900)
  await expect(page.getByText('Entry 1', { exact: true })).toBeVisible()
  expect(reads).toContain('9')
})

for (const growth of [false, true]) {
  test(`keeps rendered text through ${growth ? 'tail-growing' : 'observation-only'} resync`, async ({
    page,
  }) => {
    const api = await sessionApi(page)
    await page.route('**/api/bootstrap', (route) =>
      route.fulfill({
        json: {
          ...bootstrapFixture,
          limits: { ...bootstrapFixture.limits, max_timeline_detail_items: 1 },
        },
      }),
    )
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
    let failReads = false
    await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) => {
      textReads++
      if (failReads) return route.fulfill({ status: 503, body: 'Historical reread unavailable' })
      const first = new URL(route.request().url()).searchParams.get('first')
      if (first !== '41')
        return route.fulfill({
          json: { session_id: sessionId, items: [], projected_body_bytes: 0, continuation: null },
        })
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
          continuation: null,
        },
      })
    })
    await openSession(page)
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    failReads = true
    if (growth) api.grow()
    else api.advanceObservation()
    resynchronize()
    await expect(
      page
        .getByRole('region', { name: 'Session', exact: true })
        .getByRole('definition', { includeHidden: true })
        .filter({ hasText: /^44$/ }),
    ).toHaveText('44')
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toBeInViewport()
    await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeInViewport()
    await expect(
      page.getByRole('region', { name: 'Transcript text' }).getByRole('alert'),
    ).toContainText('Transcript failed to load')
    if (growth) expect(api.state.historyReads).toContain('after')
    expect(textReads).toBeGreaterThan(2)
  })
}

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`repository watch origin at ${viewport.name} size`, async ({ page, browserName }) => {
    await page.setViewportSize(viewport)
    const origin: WebRepositoryWatchProvenance = {
      head_branch: 'review',
      base_branch: 'main',
      dispatch_id: '00000000-0000-0000-0000-000000000063',
      action_ordinal: '2',
      repository: 'signalbox/example',
      rule_id: 'review-response',
      rule_revision: '3',
      event_id: '00000000-0000-0000-0000-000000000064',
      event_kind: 'review_submitted',
      pull_request: '81',
    }
    const api = await sessionApi(page, false, sessionId, origin)
    await openSession(page)
    await page.getByText('Session details', { exact: true }).click()
    const provenance = page.getByLabel('Repository watch')
    await expect(provenance).toContainText('Repository watch · signalbox/example #81')
    await expect(provenance).toContainText('Rule review-response v3 · Review submitted')
    await provenance.getByText('Trigger details', { exact: true }).click()
    await expect(provenance).toContainText(`Trigger ${origin.dispatch_id} · Action 2`)
    await expect(provenance).toContainText(`Event ${origin.event_id}`)
    if (browserName === 'chromium') {
      await expect(page).toHaveScreenshot(`repository-watch-origin-${viewport.name}.png`, {
        fullPage: true,
      })
    }
    api.grow()
  })
}

for (const size of ['short viewport', 'expanded textarea']) {
  test(`keeps send reachable with a ${size}`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 1000 })
    const api = await sessionApi(page)
    await openSession(page)
    const message = page.getByRole('textbox', { name: 'Message', exact: true })
    await message.fill('Continue after resizing.')
    if (size === 'short viewport') await page.setViewportSize({ width: 1440, height: 320 })
    else
      await message.evaluate((element) => {
        element.style.height = '1100px'
      })
    await page.mouse.move(700, 150)
    await page.mouse.wheel(0, 3000)
    const send = page.getByRole('button', { name: 'Send message', exact: true })
    await expect(send).toBeInViewport({ ratio: 1 })
    await send.click()
    await expect(page.getByRole('status').filter({ hasText: 'Message accepted' })).toBeVisible()
    expect(api.state.submissions).toHaveLength(1)
    expect(api.state.submissions[0]?.message).toBe('Continue after resizing.')
  })
}

test('replaces the sent notice when followed work starts', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  const composer = page.getByRole('form', { name: 'Message composer' })
  await composer.getByRole('textbox', { name: 'Message', exact: true }).fill('Start the next turn.')
  await composer.getByRole('button', { name: 'Send message', exact: true }).click()
  await expect(composer.getByRole('status')).toHaveText('Message accepted')
  api.state.active = true
  api.grow()
  await expect(composer.getByRole('status')).toHaveText(
    'Wait for the current turn to finish · Running',
  )
  await expect(composer.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
})

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`shows the stored network wait on ${viewport.name}`, async ({ page, browserName }) => {
    const errors: string[] = []
    page.on('pageerror', (error) => errors.push(error.message))
    await page.setViewportSize(viewport)
    const api = await sessionApi(page, true)
    api.state.activeState = {
      kind: 'awaiting_credential_availability',
      wait_attempt_id: turnId,
      cause: 'network_unavailable',
    }
    await page.goto(`/sessions?session=${sessionId}&workspace=true`)
    await page.getByText('Session details', { exact: true }).click()
    await expect(
      page.getByRole('status').filter({ hasText: 'Waiting for credentials' }),
    ).toHaveText('Waiting for credentials · Network unavailable')
    await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    expect(api.state.submissions).toEqual([])
    expect(errors).toEqual([])
    if (browserName === 'chromium') {
      await expect(page).toHaveScreenshot(`network-wait-${viewport.name}.png`)
    }
  })

  test(`shows supervision beside an unavailable transcript at ${viewport.name} size`, async ({
    page,
    browserName,
  }) => {
    await page.setViewportSize(viewport)
    const api = await sessionApi(page, true)
    api.state.supervision = {
      class: 'corruption',
      cause_code: 'durable_state_corruption',
      pending: true,
    }
    await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) =>
      route.fulfill({
        status: 500,
        json: {
          error: { code: 'session_projection_failed', message: 'Session projection failed' },
        },
      }),
    )
    await page.goto(`/sessions?workspace=true&session=${sessionId}`)
    await page.getByText('Session details', { exact: true }).click()
    const supervision = page.getByRole('region', { name: 'Session supervision' })
    await expect(supervision).toContainText('Recovery required')
    await expect(supervision).toContainText('corruption')
    await expect(supervision).toContainText(api.state.supervision.cause_code)
    await expect(
      page
        .getByRole('paragraph')
        .filter({ hasText: /^Recovery required$/ })
        .first(),
    ).toBeVisible()
    await expect(
      page.getByRole('region', { name: 'Transcript text' }).getByRole('alert'),
    ).toContainText('Transcript failed to load')
    await expect(
      page.getByRole('status').filter({ hasText: 'Session recovery required' }),
    ).toBeVisible()
    await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeDisabled()
    if (browserName === 'chromium')
      await expect(page).toHaveScreenshot(`session-supervision-${viewport.name}.png`, {
        fullPage: true,
      })
    api.grow()
  })
}

test('retains reconciled supervision without showing recovery pending', async ({ page }) => {
  const api = await sessionApi(page)
  api.state.supervision = { class: 'infrastructure', cause_code: 'infrastructure', pending: false }
  await openSession(page)
  await page.getByText('Session details', { exact: true }).click()
  await expect(page.getByRole('region', { name: 'Session supervision' })).toContainText(
    'Recovery recorded',
  )
  await expect(page.getByText('Recovery required', { exact: true })).toHaveCount(0)
  await expect(page.getByText('Inactive', { exact: true })).toBeVisible()
  api.grow()
})

for (const response of [204, 500]) {
  test(`keeps recovery status above a submission notice after HTTP ${response}`, async ({
    page,
  }) => {
    const api = await sessionApi(page)
    api.state.supervision = {
      class: 'infrastructure',
      cause_code: 'infrastructure',
      pending: true,
    }
    await page.route(`**/api/sessions/${sessionId}/input`, (route) =>
      route.fulfill({ status: response }),
    )
    await page.goto(`/sessions?session=${sessionId}&workspace=true`)
    await page.getByRole('textbox', { name: 'Message' }).fill('Retain the recovery status.')
    await page.getByRole('button', { name: 'Send message' }).click()
    if (response === 204) {
      await expect(page.getByRole('textbox', { name: 'Message' })).toHaveValue('')
    } else {
      await expect(page.getByRole('button', { name: 'Retry message' })).toBeEnabled()
    }
    await expect(
      page.getByRole('status').filter({ hasText: 'Session recovery required' }),
    ).toBeVisible()
  })
}

for (const active of [false, true]) {
  test(`opens a search result at its matching address when active is ${active}`, async ({
    page,
  }) => {
    const api = await sessionApi(page, active)
    await page.route('**/api/search?**', (route) =>
      route.fulfill({
        json: {
          results: [
            {
              session_id: sessionId,
              address: { event_sequence: '41' },
              projection_id: '1',
              source: { kind: 'accepted_input', accepted_input_id: turnId, turn_id: turnId },
              content_class: 'user_transcript',
              snippet: initialMessage,
              highlights: [],
            },
          ],
          continuation: null,
        },
      }),
    )
    await page.goto(`/sessions?session=${sessionId}&workspace=true`)
    await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
    await page.getByRole('row', { name: /43 Turn completed/ }).click()
    await page.getByRole('link', { name: /Search/ }).click()
    await page.getByRole('textbox', { name: 'Search text' }).fill('check')
    await page.getByRole('button', { name: 'Search', exact: true }).click()
    api.state.historyReads.length = 0
    api.state.historyAddresses.length = 0
    const result = page.getByRole('link', { name: new RegExp(initialMessage) })
    await result.focus()
    await result.press('Enter')
    await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeFocused()
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    expect(api.state.historyReads).toEqual(['around', 'around'])
    expect(api.state.historyAddresses).toEqual(['41', '41'])
    await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
    await expect(page.getByRole('row', { name: /41 Message accepted/ })).toHaveAttribute(
      'aria-selected',
      'true',
    )
    await page.getByRole('button', { name: /^Latest/ }).click()
    await expect
      .poll(() => api.state.historyReads)
      .toEqual(['around', 'around', 'latest', 'around', 'latest'])
    await expect.poll(() => new URL(page.url()).searchParams.get('around')).toBeNull()
  })
}

for (const action of ['switch', 'close', 'reopen'] as const) {
  const outcome =
    action === 'switch'
      ? 'switches sessions'
      : action === 'close'
        ? 'closes'
        : 'reopens the current session'
  test(`clears a matching address when the workspace ${outcome}`, async ({ page }) => {
    const api = await sessionApi(page)
    const otherId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e'
    const other = await sessionApi(page, false, otherId)
    await page.route('**/api/sessions?**', (route) =>
      route.fulfill({
        json: {
          cursor: '0',
          sort: 'last_activity_descending',
          summaries: [],
          continuation: null,
          total: '0',
        },
      }),
    )
    await page.goto(`/sessions?session=${sessionId}&workspace=true&around=41`)
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    const session = page.getByRole('grid', { name: 'Session timeline' })
    if (action === 'switch') {
      await openSessionFromCatalog(page, otherId)
      await expect.poll(() => other.state.historyReads).toEqual(['latest', 'latest'])
    } else if (action === 'close') {
      await session.press('Escape')
      await expect(page.getByRole('heading', { name: '0 sessions', exact: true })).toBeVisible()
    } else {
      api.state.active = true
      api.advanceObservation()
      await openSessionFromCatalog(page, sessionId)
      await expect.poll(() => api.state.historyReads.at(-1)).toBe('latest')
    }
    await expect.poll(() => new URL(page.url()).searchParams.get('around')).toBeNull()
  })
}

test('keeps an explicit non-result event visible and focused in Results mode', async ({ page }) => {
  const api = await sessionApi(page, true)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_timeline_window_items: 1 },
      },
    }),
  )
  api.grow()
  await page.addInitScript(
    ({ key, preferences }) => {
      localStorage.setItem(key, JSON.stringify(preferences))
    },
    {
      key: BROWSER_PREFERENCES_KEY,
      preferences: { ...createDefaultBrowserPreferences(), detail: 'results' },
    },
  )
  await page.goto(`/sessions?session=${sessionId}&workspace=true&around=44`)
  const timeline = page.getByRole('grid', { name: 'Session timeline' })
  const match = timeline.getByRole('row').filter({ hasText: '44' })
  await expect(match).toBeVisible()
  await expect(match).toHaveAttribute('aria-selected', 'true')
  await expect(timeline).toBeFocused()
  await page.getByRole('button', { name: /^Latest/ }).click()
  await expect.poll(() => new URL(page.url()).searchParams.get('around')).toBeNull()
  await expect(match).toHaveCount(0)
  const readsBeforeReload = api.state.historyReads.length
  await page.reload()
  await expect
    .poll(() =>
      api.state.historyReads.slice(readsBeforeReload).filter((anchor) => anchor === 'latest'),
    )
    .toEqual(['latest', 'latest'])
  expect(api.state.historyReads.slice(readsBeforeReload)).not.toContain('around')
})

test('consumes a pending search match when reopening the current session', async ({ page }) => {
  const api = await sessionApi(page, true)
  let releaseMatch = () => {}
  const matchReleased = new Promise<void>((resolve) => {
    releaseMatch = resolve
  })
  let matchRequested = false
  await page.route('**/api/sessions/*/timeline?**', async (route) => {
    if (new URL(route.request().url()).searchParams.get('anchor') === 'around') {
      matchRequested = true
      await matchReleased
    }
    await route.fallback()
  })
  await page.goto(`/sessions?session=${sessionId}&workspace=true&around=43`)
  await expect.poll(() => matchRequested).toBe(true)
  await openSessionFromCatalog(page, sessionId)
  await expect.poll(() => new URL(page.url()).searchParams.get('around')).toBeNull()
  releaseMatch()
  await expect.poll(() => api.state.historyReads.at(-1)).toBe('latest')
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeVisible()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(page.getByRole('row', { name: /43 Turn completed/ })).toHaveAttribute(
    'aria-selected',
    'false',
  )
})

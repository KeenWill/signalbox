import { expect, type Page, type TestInfo, test } from '@playwright/test'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 65_536 },
} as const

const repository = 'signalbox/operator'
const sessionOne = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c71'
const sessionTwo = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c72'
const event = (ordinal: number, kind = 'head_changed') => ({
  cursor_generation: '17',
  event_ordinal: ordinal,
  id: `event-${ordinal}`,
  kind,
  observed_at_unix_milliseconds: `${1_787_342_400_000 - ordinal * 1_000}`,
  pull_request: `${100 + (ordinal % 4)}`,
})
const dispatch = {
  attempted_at_unix_milliseconds: '1787342399000',
  event_id: 'event-5',
  id: 'dispatch-5',
  rule: 'review-convergence',
}
const settlement = {
  dispatch_id: 'dispatch-4',
  event_id: 'event-4',
  settled_at_unix_milliseconds: '1787342399500',
}

const repositoriesFixture = {
  continuation_after_repository: null,
  repositories: [
    {
      cursor_generation: '17',
      event_kind_counts_previous_hour: [
        { count: '101', kind: 'head_changed' },
        { count: '4', kind: 'review_submitted' },
      ],
      held_slot_count: '1',
      last_actionable_event: event(4, 'review_submitted'),
      last_automation_settlement: settlement,
      last_dispatch_attempt: dispatch,
      last_observed_event: event(5),
      latest_projection_latency_milliseconds: '37',
      latest_webhook: {
        action_name: 'synchronize',
        event_name: 'pull_request',
        receipt_sequence: '101',
        received_at_unix_milliseconds: '1787342400000',
      },
      maximum_projection_latency_milliseconds_previous_hour: '83',
      observed_at_unix_milliseconds: '1787342400000',
      previous_five_minutes: {
        projected: '101',
        quarantined: '0',
        received: '101',
        seconds: 300,
        terminal: '101',
      },
      previous_hour: {
        projected: '402',
        quarantined: '1',
        received: '403',
        seconds: 3_600,
        terminal: '403',
      },
      queued_obligation_count: '1',
      repository,
    },
  ],
} as const

const pullRequest = (
  number: string,
  title: string,
  automation: Record<string, string>,
  commissionedSessionCount = '0',
) => ({
  automation,
  base_branch: 'main',
  checks: number === '102' ? 'failing' : 'passing',
  commissioned_session_count: commissionedSessionCount,
  draft: 'ready_for_review',
  head: `${number}`.repeat(40).slice(0, 40),
  head_branch: `agent/pr-${number}`,
  head_repository: repository,
  held_slot_count: number === '101' ? '1' : '0',
  last_actionable_event: event(Number(number) - 99),
  last_automation_settlement: number === '100' ? settlement : null,
  last_dispatch_attempt: number === '100' ? dispatch : null,
  last_observed_event: event(Number(number) - 98),
  lifecycle: 'open',
  mergeable: number === '102' ? 'conflicting' : 'mergeable',
  number,
  open_child_count: number === '100' ? '1' : '0',
  open_parent: number === '101' ? '100' : null,
  queued_obligation_count: number === '101' ? '1' : '0',
  review_decision: number === '102' ? 'changes_requested' : 'approved',
  stale_review_count: number === '102' ? '1' : '0',
  title,
  unresolved_thread_count: number === '101' ? '2' : '0',
})

const pullRequestsFixture = {
  continuation_after_pull_request: null,
  pull_requests: [
    pullRequest('100', 'Converged release', {
      dispatch_id: 'dispatch-4',
      kind: 'current_head_sealed',
      sealed_event_id: 'event-4',
      settled_at_unix_milliseconds: '1787342399500',
    }),
    pullRequest('101', 'Blocked review follow-up', {
      dispatch_id: 'dispatch-5',
      kind: 'held',
    }),
    pullRequest('102', 'Stale approval evidence', {
      dispatch_id: 'dispatch-6',
      kind: 'stale_seal',
      sealed_event_id: 'event-2',
    }),
    pullRequest(
      '103',
      'Non-converged checks',
      { dispatch_id: 'dispatch-7', kind: 'non_converged' },
      '2',
    ),
  ],
  repository,
} as const

const workFixture = {
  held_continuation_after: null,
  held_slots: [
    {
      blockers: ['undelivered_action', 'pursuing_goal'],
      dispatch_id: 'dispatch-5',
      held_since_unix_milliseconds: '9999999999999',
      scope: { kind: 'stack', repository, root_pull_request: '101' },
      rule: 'review-convergence',
      session_ids: [sessionOne],
    },
  ],
  obligation_continuation_after: null,
  queued_obligations: [
    {
      failed_attempts: '1',
      first_event_id: 'event-3',
      id: 'obligation-1',
      latest_event_id: 'event-5',
      latest_match_at_unix_milliseconds: '1787342400000',
      matched_event_count: '3',
      owed_since_unix_milliseconds: '9999999999999',
      scope: { kind: 'pull_request', repository, number: '101' },
      readiness: { eligible_at_unix_milliseconds: '1787342460000', kind: 'cooldown' },
      rule: 'review-convergence',
    },
  ],
} as const

const sessionsFixture = {
  continuation_before: null,
  sessions: [
    {
      attention: {
        action: 'provide_goal_need',
        current_turn_id: 'turn-one',
        goal_block: {
          generation: '2',
          need_summary: 'Choose the acceptable review disposition.',
          reason: 'user_input_required',
        },
        judge: { actionable: '1', completed: '8', escalated: '0', failed: '0' },
        last_activity: { kind: 'goal', unix_milliseconds: '1787342400000' },
        session_id: sessionOne,
        state: 'blocked',
      },
      commissioned_at_unix_milliseconds: '1787342200000',
      purpose: {
        dispatch_id: 'dispatch-7',
        event_id: 'event-5',
        kind: 'rule_dispatch',
        rule: 'review-convergence',
        template: 'resolve-review',
      },
    },
    {
      attention: {
        action: null,
        current_turn_id: null,
        goal_block: null,
        judge: { actionable: '0', completed: '5', escalated: '0', failed: '0' },
        last_activity: { kind: 'session', unix_milliseconds: '1787342100000' },
        session_id: sessionTwo,
        state: 'idle',
      },
      commissioned_at_unix_milliseconds: '1787342100000',
      purpose: {
        dispatch_id: 'dispatch-8',
        kind: 'operator_commission',
        template: 'inspect-checks',
      },
    },
  ],
} as const

const firstActivityFixture = {
  event_continuation_before: null,
  events: [event(5), event(4), event(3), event(2), event(1)],
  webhook_continuation_before_receipt_sequence: '2',
  webhooks: Array.from({ length: 100 }, (_, index) => ({
    action_name: index % 2 === 0 ? 'synchronize' : 'submitted',
    disposition: index === 99 ? 'quarantined' : 'projected',
    event_name: index % 2 === 0 ? 'pull_request' : 'pull_request_review',
    latest_projected_at_unix_milliseconds: `${1_787_342_400_050 - index * 10}`,
    projection_count: index === 99 ? '0' : '1',
    receipt_sequence: `${101 - index}`,
    received_at_unix_milliseconds: `${1_787_342_400_000 - index * 10}`,
  })),
} as const

const secondActivityFixture = {
  event_continuation_before: null,
  events: [],
  webhook_continuation_before_receipt_sequence: null,
  webhooks: [
    {
      action_name: 'opened',
      disposition: 'projected',
      event_name: 'pull_request',
      latest_projected_at_unix_milliseconds: '1787342398005',
      projection_count: '1',
      receipt_sequence: '1',
      received_at_unix_milliseconds: '1787342398000',
    },
  ],
} as const

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const installActivityScenario = async (page: Page) => {
  const apiRequests: string[] = []
  page.on('request', (request) => {
    const url = new URL(request.url())
    if (url.pathname.startsWith('/api/')) apiRequests.push(`${url.pathname}${url.search}`)
  })
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/repository-watch/repositories**', (route) =>
    route.fulfill({ json: repositoriesFixture }),
  )
  await page.route('**/api/repository-watch/pull-requests**', (route) =>
    route.fulfill({ json: pullRequestsFixture }),
  )
  await page.route('**/api/repository-watch/work**', (route) =>
    route.fulfill({ json: workFixture }),
  )
  await page.route('**/api/repository-watch/sessions**', (route) =>
    route.fulfill({ json: sessionsFixture }),
  )
  await page.route('**/api/repository-watch/activity**', (route) => {
    const url = new URL(route.request().url())
    return route.fulfill({
      json: url.searchParams.has('webhook_before_receipt_sequence')
        ? secondActivityFixture
        : firstActivityFixture,
    })
  })
  return apiRequests
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('pages a 101-delivery burst and preserves semantic session evidence', async ({ page }) => {
  const problems = watchBrowser(page)
  const apiRequests = await installActivityScenario(page)
  await page.goto('/activity')

  await expect(page.getByRole('heading', { name: repository })).toBeVisible()
  await expect(page.getByText('37 / 83 ms')).toBeVisible()
  await expect(page.getByText('current head sealed', { exact: false })).toBeVisible()
  await expect(page.getByText('held · 1 held', { exact: false })).toBeVisible()
  await expect(page.getByText('stale seal', { exact: false })).toBeVisible()
  await expect(page.getByText('non converged', { exact: false })).toBeVisible()
  await expect(page.getByText('105 loaded in browser window')).toBeVisible()
  const historyViewport = page.getByRole('rowgroup', {
    name: 'Scrollable repository activity rows',
  })
  await historyViewport.focus()
  await expect(historyViewport).toBeFocused()
  await historyViewport.press('End')
  await expect(page.getByText('delivery 2', { exact: true })).toBeVisible()

  await page.getByRole('button', { name: /#103 Non-converged checks/ }).click()
  await expect(page.getByText(sessionOne)).toBeVisible()
  await expect(page.getByText(sessionTwo)).toBeVisible()
  const sessionEvidence = page.getByText(sessionOne, { exact: true })
  await expect(sessionEvidence).toBeVisible()
  await expect(sessionEvidence).not.toHaveAttribute('href')
  await page.getByRole('button', { name: /#103 Non-converged checks/ }).click()
  await expect(page.getByRole('region', { name: 'Commissioned sessions' })).toBeHidden()
  await page.getByRole('button', { name: /#103 Non-converged checks/ }).click()

  await page.getByRole('button', { name: 'Load older window' }).click()
  await expect(page.getByText('106 loaded in browser window')).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Events and webhooks' })).toBeFocused()
  await page.getByRole('button', { name: 'Return to latest' }).click()
  await expect(page.getByRole('heading', { name: 'Events and webhooks' })).toBeFocused()
  expect(apiRequests).toContain(
    `/api/repository-watch/activity?repository=signalbox%2Foperator&include_events=false&include_webhooks=true&webhook_before_receipt_sequence=2`,
  )
  expect(apiRequests.every((request) => request.startsWith('/api/'))).toBe(true)
  expect(apiRequests.join(' ')).not.toMatch(/postgres|database|sql/)

  await page.keyboard.press('g')
  await page.keyboard.press('t')
  await expect(page).toHaveURL(/\/activity$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the bounded repository operations workstation', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installActivityScenario(page)
  await page.goto('/activity')
  await page.getByRole('button', { name: /#103 Non-converged checks/ }).click()
  await expect(page.getByText(sessionOne)).toBeVisible()
  await expect(page).toHaveScreenshot('activity-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the activity surface on a narrow viewport', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installActivityScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/activity')
  await expect(page.getByRole('heading', { name: repository })).toBeVisible()
  await expect(page).toHaveScreenshot('activity-mobile-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

import { expect, type Page, type TestInfo, test } from '@playwright/test'

interface RouteEvidence {
  path: string
  title: string
  snapshot: string
}

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_session_timeline: true,
    bounded_session_timeline_detail: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_timeline_detail_items: 128,
    max_timeline_detail_bytes: 65_536,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
  },
} as const

const toolEvidenceArguments = '{"path":"docs/spec/sessions-and-transcript.md"}'
const toolEvidenceResult = 'Read 64 bounded lines from the owning contract.'
const encodedBytes = (value: string) => new TextEncoder().encode(value).byteLength
const timelineHeaderBytes = (kind: string) => 64 + encodedBytes(kind)
const timelineDetailBytes = (text = '') => 128 + encodedBytes(text)

const sessionEvidenceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  itemCount: '1000000',
  detailAddress: '999998',
  detail: {
    session_id: '00000000-0000-0000-0000-000000000991',
    items: [
      {
        address: { event_sequence: '999998' },
        kind: 'tool_batch_transition',
        body: {
          type: 'tool_batch',
          turn_id: '00000000-0000-0000-0000-000000999998',
          producing_model_call_id: '00000000-0000-0000-0000-000000999898',
          state: 'proposed',
          tools: [
            {
              request_id: '00000000-0000-0000-0000-000000999798',
              tool_name: 'workspace_read',
              approval_posture: 'auto',
              approval_judge_escalated: false,
              operator_required: false,
              arguments: {
                text: toolEvidenceArguments,
                offset_bytes: '0',
                total_bytes: String(new TextEncoder().encode(toolEvidenceArguments).byteLength),
                continuation: null,
              },
              attempt_id: '00000000-0000-0000-0000-000000999698',
              state: 'completed',
              effect_posture: 'read_only',
              sandbox_posture: 'workspace_read',
              result: null,
              failure: null,
              cause_code: null,
            },
          ],
          goal_events: [],
        },
        projected_body_bytes: timelineDetailBytes(toolEvidenceArguments),
      },
    ],
    projected_body_bytes: timelineDetailBytes(toolEvidenceArguments),
    continuation: {
      type: 'more_body',
      body: {
        address: { event_sequence: '999998' },
        field: 'tool_result',
        member_index: 0,
        offset_bytes: '0',
      },
    },
  },
  detailResult: {
    session_id: '00000000-0000-0000-0000-000000000991',
    items: [
      {
        address: { event_sequence: '999998' },
        kind: 'tool_batch_transition',
        body: {
          type: 'tool_batch',
          turn_id: '00000000-0000-0000-0000-000000999998',
          producing_model_call_id: '00000000-0000-0000-0000-000000999898',
          state: 'results_projected',
          tools: [
            {
              request_id: '00000000-0000-0000-0000-000000999798',
              tool_name: 'workspace_read',
              approval_posture: 'auto',
              approval_judge_escalated: false,
              operator_required: false,
              arguments: null,
              attempt_id: '00000000-0000-0000-0000-000000999698',
              state: 'completed',
              effect_posture: 'read_only',
              sandbox_posture: 'workspace_read',
              result: {
                text: toolEvidenceResult,
                offset_bytes: '0',
                total_bytes: String(new TextEncoder().encode(toolEvidenceResult).byteLength),
                continuation: null,
              },
              failure: null,
              cause_code: null,
            },
          ],
          goal_events: [],
        },
        projected_body_bytes: timelineDetailBytes(toolEvidenceResult),
      },
    ],
    projected_body_bytes: timelineDetailBytes(toolEvidenceResult),
    continuation: {
      type: 'more_body',
      body: {
        address: { event_sequence: '999998' },
        field: 'goal_text',
        member_index: 0,
        offset_bytes: '0',
      },
    },
  },
  detailGoal: {
    session_id: '00000000-0000-0000-0000-000000000991',
    items: [
      {
        address: { event_sequence: '999998' },
        kind: 'tool_batch_transition',
        body: {
          type: 'tool_batch',
          turn_id: '00000000-0000-0000-0000-000000999998',
          producing_model_call_id: '00000000-0000-0000-0000-000000999898',
          state: 'results_projected',
          tools: [],
          goal_events: [
            {
              event_kind: 'advanced',
              generation: '19',
              reason: 'evidence inspected',
              text: null,
            },
          ],
        },
        projected_body_bytes: timelineDetailBytes(),
      },
    ],
    projected_body_bytes: timelineDetailBytes(),
    continuation: null,
  },
} as const

const attentionEvidence = { path: '/attention', title: 'Attention', snapshot: 'attention' } as const
const sessionsEvidence = { path: '/sessions', title: 'Sessions', snapshot: 'sessions' } as const
const searchEvidence = { path: '/search', title: 'Search', snapshot: 'search' } as const
const activityEvidence = { path: '/activity', title: 'Activity', snapshot: 'activity' } as const
const runnersEvidence = { path: '/runners', title: 'Runners', snapshot: 'runners' } as const
const reviewsEvidence = { path: '/reviews', title: 'Reviews', snapshot: 'reviews' } as const
const importsEvidence = { path: '/imports', title: 'Imports', snapshot: 'imports' } as const
const usageEvidence = { path: '/usage', title: 'Usage', snapshot: 'usage' } as const
const settingsEvidence = { path: '/settings', title: 'Settings', snapshot: 'settings' } as const

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const useDeterministicSession = (page: Page) =>
  page.route('**/api/sessions/**', (route) => {
    const path = new URL(route.request().url()).pathname
    if (path.endsWith(`/${sessionEvidenceFixture.detailAddress}/detail`)) {
      const field = new URL(route.request().url()).searchParams.get('cursor_field')
      return route.fulfill({
        json:
          field === 'tool_result'
            ? sessionEvidenceFixture.detailResult
            : field === 'goal_text'
              ? sessionEvidenceFixture.detailGoal
              : sessionEvidenceFixture.detail,
      })
    }
    if (path.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionEvidenceFixture.id,
          items: [
            {
              address: { event_sequence: '999998' },
              kind: sessionEvidenceFixture.detail.items[0].kind,
              projected_structured_bytes: timelineHeaderBytes('tool_batch_transition'),
            },
            {
              address: { event_sequence: '999999' },
              kind: 'turn_activated',
              projected_structured_bytes: timelineHeaderBytes('turn_activated'),
            },
            {
              address: { event_sequence: '1000000' },
              kind: 'turn_completed',
              projected_structured_bytes: timelineHeaderBytes('turn_completed'),
            },
          ],
          projected_structured_bytes:
            timelineHeaderBytes('tool_batch_transition') +
            timelineHeaderBytes('turn_activated') +
            timelineHeaderBytes('turn_completed'),
          continuation_before: { event_sequence: '999998' },
          continuation_after: null,
        },
      })
    }
    return route.fulfill({
      json: {
        session_id: sessionEvidenceFixture.id,
        sizes: {
          item_count: sessionEvidenceFixture.itemCount,
          projected_text_bytes: '48000000',
          projected_structured_bytes: '96000000',
          referenced_blob_count: '24000',
          referenced_blob_bytes: '96000000000',
        },
        first_address: { event_sequence: '1' },
        latest_address: { event_sequence: '1000000' },
        work: { active_turn_count: '1', queued_turn_count: '4' },
        observed_through: '1000037',
      },
    })
  })

const captureRouteEvidence = async (page: Page, evidence: RouteEvidence) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(evidence.path)
  await expect(page.getByRole('heading', { name: evidence.title, level: 1 })).toBeVisible()
  await expect(page).toHaveScreenshot(`${evidence.snapshot}-desktop-dark.png`, {
    animations: 'disabled',
  })

  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot(`${evidence.snapshot}-desktop-light.png`, {
    animations: 'disabled',
  })

  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await expect(page).toHaveScreenshot(`${evidence.snapshot}-mobile-light.png`, {
    animations: 'disabled',
  })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
}

const captureSessionEvidence = async (page: Page) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(sessionsEvidence.path)
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(sessionEvidenceFixture.id)
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await expect(page.getByRole('heading', { name: sessionEvidenceFixture.id })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  const detail = page.getByRole('button', {
    name: new RegExp(
      `${sessionEvidenceFixture.detailAddress} ${sessionEvidenceFixture.detail.items[0].kind.replaceAll('_', ' ')}`,
    ),
  })
  await detail.focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('heading', {
      name: sessionEvidenceFixture.detail.items[0].body.tools[0].tool_name,
    }),
  ).toBeVisible()
  const continueDetail = page.getByRole('button', { name: 'Load next bounded detail chunk' })
  await continueDetail.click()
  await expect(page.getByText(toolEvidenceResult, { exact: true })).toBeVisible()
  await continueDetail.click()
  await expect(page.getByText('evidence inspected', { exact: true })).toBeVisible()
  await expect(page).toHaveScreenshot('sessions-detail-desktop-dark.png', {
    animations: 'disabled',
  })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot('sessions-detail-desktop-light.png', {
    animations: 'disabled',
  })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await expect(page).toHaveScreenshot('sessions-mobile-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
}

test('captures Attention route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, attentionEvidence)
})

test('captures Sessions route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureSessionEvidence(page)
})

test('captures Search route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, searchEvidence)
})

test('captures Activity route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, activityEvidence)
})

test('captures Runners route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, runnersEvidence)
})

test('captures Reviews route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, reviewsEvidence)
})

test('captures Imports route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, importsEvidence)
})

test('captures Usage route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, usageEvidence)
})

test('captures Settings route evidence', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  await captureRouteEvidence(page, settingsEvidence)
})

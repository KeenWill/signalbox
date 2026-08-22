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
    bounded_session_live: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 65_536,
    max_session_live_queued_turns: 32,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
  },
} as const

const sessionEvidenceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  itemCount: '1000000',
} as const

const sessionEvidenceSummary = {
  active_turn_count: '1',
  archived: false,
  current_turn_id: '00000000-0000-0000-0000-000000000041',
  judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
  last_activity: { kind: 'turn', unix_milliseconds: '1787400000000' },
  queued_turn_count: '4',
  session_id: sessionEvidenceFixture.id,
  state: 'awaiting_approval',
  title_summary: 'Release train approval',
  title_truncated: false,
} as const

const sessionEvidenceLive = {
  active: {
    state: {
      kind: 'awaiting_tool_approval',
      tool_request_id: '00000000-0000-0000-0000-000000000071',
    },
    turn_id: '00000000-0000-0000-0000-000000000041',
  },
  observed_through: '1000037',
  queued_turn_count: '4',
  queued_turn_ids: [
    '00000000-0000-0000-0000-000000000051',
    '00000000-0000-0000-0000-000000000052',
    '00000000-0000-0000-0000-000000000053',
    '00000000-0000-0000-0000-000000000054',
  ],
  reconciliation: null,
  runner: {
    connection_health: 'connected',
    placement_revision: '7',
    runner_id: '00000000-0000-0000-0000-000000000061',
    state: 'pinned',
  },
  session_id: sessionEvidenceFixture.id,
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

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions**', (route) =>
    new URL(route.request().url()).pathname === '/api/sessions'
      ? route.fulfill({
          json: {
            continuation: null,
            cursor: '0',
            sort: 'last_activity_descending',
            summaries: [],
            total: '0',
          },
        })
      : route.fallback(),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '0',
          sort: 'last_activity_descending',
          summaries: [],
          total: '0',
        },
      })}\n`,
    }),
  )
}

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const useDeterministicSession = async (page: Page) => {
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({
        kind: 'snapshot',
        snapshot: {
          continuation: null,
          cursor: '1000037',
          sort: 'last_activity_descending',
          summaries: [sessionEvidenceSummary],
          total: '1000',
        },
      })}\n`,
    }),
  )
  await page.route('**/api/sessions**', (route) => {
    const pathname = new URL(route.request().url()).pathname
    if (pathname === '/api/sessions') {
      return route.fulfill({
        json: {
          continuation: null,
          cursor: '1000037',
          sort: 'last_activity_descending',
          summaries: [sessionEvidenceSummary],
          total: '1000',
        },
      })
    }
    if (pathname.endsWith('/follow')) {
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: sessionEvidenceLive })}\n`,
      })
    }
    if (pathname.endsWith('/live')) return route.fulfill({ json: sessionEvidenceLive })
    if (pathname.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionEvidenceFixture.id,
          items: [
            {
              address: { event_sequence: '999998' },
              kind: 'tool_batch_transition',
              projected_structured_bytes: 85,
            },
            {
              address: { event_sequence: '999999' },
              kind: 'turn_activated',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '1000000' },
              kind: 'turn_completed',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: 241,
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
}

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
  await page.getByRole('option', { name: /Release train approval/ }).click()
  await expect(page.getByRole('heading', { name: 'Release train approval' })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(page).toHaveScreenshot('sessions-desktop-dark.png', { animations: 'disabled' })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot('sessions-desktop-light.png', { animations: 'disabled' })
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

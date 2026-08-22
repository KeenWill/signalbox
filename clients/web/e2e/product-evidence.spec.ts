import { expect, type Page, type TestInfo, test } from '@playwright/test'
import {
  richItemPage,
  richRegionPage,
  richSessionId,
  richTimelineWindow,
  richTurnId,
  richTurnPage,
} from './session-detail-fixture'

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
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
    max_timeline_detail_items: 128,
    max_timeline_detail_bytes: 65_536,
  },
} as const

const sessionEvidenceFixture = {
  id: richSessionId,
  itemCount: '1000000',
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
    const url = new URL(route.request().url())
    if (url.pathname.includes(`/turns/${richTurnId}/`))
      return route.fulfill({ json: richTurnPage() })
    if (url.pathname.endsWith('/timeline-detail')) {
      return route.fulfill({ json: richRegionPage(url.searchParams.has('cursor_address')) })
    }
    if (url.pathname.endsWith('/detail')) {
      return route.fulfill({ json: richItemPage(url.pathname.split('/').at(-2) ?? '') })
    }
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: richTimelineWindow })
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
        first_address: { event_sequence: '89' },
        latest_address: { event_sequence: '101' },
        work: { active_turn_count: '1', queued_turn_count: '4' },
        observed_through: '101',
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
  await page.getByRole('button', { name: 'Inspect loaded region' }).click()
  await expect(page.getByText('Denied: the release window has closed.')).toBeVisible()
  await page.getByText('provider_boundary_lost_after_send').scrollIntoViewIfNeeded()
  await expect(page).toHaveScreenshot('sessions-desktop-dark.png', { animations: 'disabled' })
  await expect(
    page.locator('.timeline-detail-card').filter({ hasText: 'model call transition' }),
  ).toHaveScreenshot('sessions-detail-desktop-dark.png', { animations: 'disabled' })
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await page.getByText('Denied: the release window has closed.').scrollIntoViewIfNeeded()
  await expect(page).toHaveScreenshot('sessions-desktop-light.png', { animations: 'disabled' })
  await expect(
    page.locator('.timeline-detail-card').filter({ hasText: 'tool approval decided' }),
  ).toHaveScreenshot('sessions-detail-desktop-light.png', { animations: 'disabled' })
  await page.getByRole('button', { name: 'Continue region' }).click()
  await expect(
    page.getByText('Earlier investigation evidence compacted through transcript position 88.'),
  ).toBeVisible()
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await page
    .getByText('Earlier investigation evidence compacted through transcript position 88.')
    .scrollIntoViewIfNeeded()
  await expect(page).toHaveScreenshot('sessions-mobile-light.png', { animations: 'disabled' })
  await expect(
    page.locator('.timeline-detail-card').filter({ hasText: 'context compacted' }),
  ).toHaveScreenshot('sessions-detail-mobile-light.png', { animations: 'disabled' })
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

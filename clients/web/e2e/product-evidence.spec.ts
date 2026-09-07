import { expect, type Page, type TestInfo, test } from '@playwright/test'
// The shared fixture is the single copy kept aligned with WebContractBootstrap::current();
// readBootstrap now rejects any bootstrap whose limits contradict it.
import { webContractBootstrapFixture } from '../src/product.fixture'
import { useDeterministicImportApi } from './import-api-fixture'

interface RouteEvidence {
  path: string
  title: string
  snapshot: string
}

const sessionEvidenceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  itemCount: '1000000',
} as const

const attentionEvidence = { path: '/attention', title: 'Attention', snapshot: 'attention' } as const
const sessionsEvidence = { path: '/sessions', title: 'Sessions', snapshot: 'sessions' } as const
const searchEvidence = { path: '/search', title: 'Search', snapshot: 'search' } as const
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

const emptyAttentionFixture = {
  continuation_after_session_id: null,
  cursor: '0',
  summaries: [],
} as const

const useDeterministicBootstrap = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: emptyAttentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => route.fulfill({ json: emptyAttentionFixture }))
}

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
    if (new URL(route.request().url()).pathname.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionEvidenceFixture.id,
          // Item charges follow the wire contract: a 64-byte envelope plus the UTF-8
          // event-kind spelling (21 bytes for tool_batch_transition, 14 for the others).
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
          projected_structured_bytes: '78000000',
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
  // Only the Imports route reads this adapter; every other route ignores it.
  await useDeterministicImportApi(page)
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
  await page.goto(`${sessionsEvidence.path}?workspace=true`)
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(sessionEvidenceFixture.id)
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await expect(page.getByRole('heading', { name: sessionEvidenceFixture.id })).toBeVisible()
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

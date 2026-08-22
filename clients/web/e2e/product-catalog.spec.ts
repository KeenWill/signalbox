import { expect, type Page, type TestInfo, test } from '@playwright/test'

const firstSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const secondSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e'
const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_timeline_window_bytes: 524_288,
    max_timeline_window_items: 256,
  },
} as const
const firstPage = {
  continuation: {
    kind: 'last_activity',
    session_id: secondSessionId,
    unix_microseconds: '1724194800000000',
  },
  cursor: '18',
  sort: 'last_activity_descending',
  summaries: [
    {
      action: null,
      active_turn_count: '1',
      archived: false,
      current_turn_id: 'turn-31',
      goal_block: null,
      judge: { actionable: '0', completed: '3', escalated: '0', failed: '0' },
      last_activity: { kind: 'turn', unix_milliseconds: '1724200000000' },
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
      last_activity: { kind: 'goal', unix_milliseconds: '1724194800000' },
      queued_turn_count: '0',
      session_id: secondSessionId,
      state: 'blocked',
      title_summary: 'Deployment decision',
      title_truncated: false,
    },
  ],
  total: '48',
} as const
const secondPage = {
  continuation: null,
  cursor: '18',
  sort: 'last_activity_descending',
  summaries: [
    {
      action: null,
      active_turn_count: '0',
      archived: true,
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
      last_activity: { kind: 'session', unix_milliseconds: '1724100000000' },
      queued_turn_count: '0',
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c8f',
      state: 'idle',
      title_summary: 'Archived investigation',
      title_truncated: false,
    },
  ],
  total: '48',
} as const

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const useCatalogFixture = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/sessions?**', (route) => {
    const request = new URL(route.request().url())
    const response = request.searchParams.has('after_session_id') ? secondPage : firstPage
    return route.fulfill({ json: response })
  })
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('filters and inspects a session without a mouse, then restores focus', async ({ page }) => {
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: `${firstPage.total} sessions` })).toBeVisible()

  await page.getByRole('textbox', { name: 'Search titles' }).fill('release')
  await page.getByRole('textbox', { name: 'Search titles' }).press('Enter')
  await expect(page).toHaveURL(/q=release/)
  const session = page.getByRole('button', { name: firstPage.summaries[0].title_summary })
  await session.focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('heading', { name: firstPage.summaries[0].title_summary, level: 2 }),
  ).toBeVisible()
  await expect(page.getByRole('button', { name: 'Close session inspector' })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(
    page.getByRole('heading', { name: firstPage.summaries[0].title_summary, level: 2 }),
  ).toBeHidden()
  await expect(session).toBeFocused()
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
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures desktop dark, desktop light, and responsive catalog evidence', async ({
  page,
}, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useCatalogFixture(page)
  await page.goto(`/sessions?session=${firstSessionId}`)
  await expect(
    page.getByRole('heading', { name: firstPage.summaries[0].title_summary, level: 2 }),
  ).toBeVisible()
  await expect(page).toHaveScreenshot('catalog-desktop-dark.png', { animations: 'disabled' })

  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot('catalog-desktop-light.png', { animations: 'disabled' })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await expect(page).toHaveScreenshot('catalog-mobile-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

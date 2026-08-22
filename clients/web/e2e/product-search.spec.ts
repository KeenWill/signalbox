import { expect, type Page, type TestInfo, test } from '@playwright/test'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_lexical_search: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_search_query_bytes: 512,
    max_search_page_items: 100,
    max_search_snippet_bytes: 512,
    max_timeline_window_bytes: 524_288,
    max_timeline_window_items: 256,
  },
} as const
const firstPage = {
  results: [
    {
      session_id: sessionId,
      address: { event_sequence: '901' },
      source: {
        kind: 'accepted_input',
        accepted_input_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5010',
        turn_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5020',
      },
      content_class: 'user_transcript',
      snippet: 'durable release evidence',
      highlights: [{ start_byte: 0, end_byte: 7 }],
    },
    {
      session_id: sessionId,
      address: { event_sequence: '750' },
      source: {
        kind: 'derived_artifact',
        artifact_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5030',
      },
      content_class: 'derived_text_artifact',
      snippet: 'derived artifact context',
      highlights: [{ start_byte: 8, end_byte: 16 }],
    },
  ],
  continuation: { address: { event_sequence: '750' }, projection_id: '42' },
} as const
const secondPage = {
  results: [
    {
      session_id: sessionId,
      address: { event_sequence: '112' },
      source: { kind: 'session', session_id: sessionId },
      content_class: 'session_metadata',
      snippet: 'release planning',
      highlights: [{ start_byte: 0, end_byte: 7 }],
    },
  ],
  continuation: null,
} as const

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const useSearchFixture = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    const request = new URL(route.request().url())
    const pageFixture = request.searchParams.has('after_address') ? secondPage : firstPage
    return route.fulfill({ json: pageFixture })
  })
}

const submitSearch = async (page: Page) => {
  await page.getByRole('textbox', { name: 'Search text' }).fill('release evidence')
  await page.getByRole('textbox', { name: /Exact session/ }).fill(sessionId)
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('searches without advertising an unavailable session reveal', async ({ page }) => {
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search')
  await submitSearch(page)

  await expect(page).toHaveURL(/q=release(?:\+|%20)evidence/)
  await expect(page.getByText('durable release evidence')).toContainText('release')
  await expect(page.getByRole('link', { name: 'Reveal in session' })).toHaveCount(0)
  await expect(page.getByText('Session reveal unavailable').first()).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not announce query validation before bootstrap limits load', async ({ page }) => {
  await page.route('**/api/bootstrap', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 250))
    await route.fulfill({ json: bootstrapFixture })
  })
  await page.goto('/search?q=release')

  await expect(page.getByRole('alert')).toHaveCount(0)
})

test('preserves JSON-shaped lexical text in deep-link URLs', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=%7B%22term%22%3A%22release%22%7D')

  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveValue('{"term":"release"}')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
})

test('replaces the bounded result page through its typed cursor', async ({ page }) => {
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  const nextPage = page.getByRole('button', { name: 'Next page' })
  await nextPage.focus()
  await nextPage.click()
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeVisible()
  await expect(page).toHaveURL(/afterAddress=750/)
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeFocused()
  await expect(page.getByText('release planning')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves search focus and announces asynchronous results', async ({ page }) => {
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('release evidence')
  await search.press('Enter')
  await expect(search).toBeFocused()
  await expect(page.getByText('2 results loaded on this page.', { exact: true })).toHaveText(
    '2 results loaded on this page.',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('reports an unreachable search transport separately from contract decoding', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => route.abort('connectionrefused'))
  await page.goto('/search?q=release')

  await expect(page.getByRole('alert')).toContainText(
    'The search request could not reach Signalbox.',
  )
  expect(problems.pageErrors).toEqual([])
})

test('captures desktop dark, desktop light, and responsive search evidence', async ({
  page,
}, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await expect(page).toHaveScreenshot('search-desktop-dark.png', { animations: 'disabled' })

  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect(page).toHaveScreenshot('search-desktop-light.png', { animations: 'disabled' })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await expect(page).toHaveScreenshot('search-mobile-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

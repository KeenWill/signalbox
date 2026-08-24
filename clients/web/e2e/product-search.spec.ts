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
      projection_id: '84',
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
      projection_id: '42',
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
      projection_id: '21',
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

const useRecoveringSearchFixture = async (page: Page) => {
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    attempts += 1
    return attempts === 1
      ? route.fulfill({
          status: 503,
          json: {
            error: { code: 'temporary', kind: 'transport', message: 'temporary failure' },
          },
        })
      : route.fulfill({ json: firstPage })
  })
}

const useFailingPaginationFixture = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    const request = new URL(route.request().url())
    return request.searchParams.has('after_address')
      ? route.fulfill({
          status: 503,
          json: {
            error: { code: 'temporary', kind: 'transport', message: 'temporary failure' },
          },
        })
      : route.fulfill({ json: firstPage })
  })
}

const useRefreshingSearchFixture = async (page: Page) => {
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', async (route) => {
    attempts += 1
    if (attempts > 1) await new Promise((resolve) => setTimeout(resolve, 250))
    return route.fulfill({ json: attempts === 1 ? firstPage : secondPage })
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
  await expect(page).toHaveURL(
    new RegExp(`afterAddress=${firstPage.continuation.address.event_sequence}`),
  )
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeFocused()
  await expect(page.getByText('release planning')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus when pagination fails', async ({ page }) => {
  await useFailingPaginationFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  await page.getByRole('button', { name: 'Next page' }).click()

  await expect(page.getByRole('heading', { name: 'Search could not be read' })).toBeFocused()
})

test('resets pagination when submitting a different search scope', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  await page.getByRole('button', { name: 'Next page' }).click()
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeVisible()
  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('different scope')
  await search.press('Enter')

  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await expect(page).toHaveURL(/q=different(?:\+|%20)scope/)
  await expect(page).not.toHaveURL(/afterAddress=/)
})

test('synchronizes pagination with browser history', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  await page.getByRole('button', { name: 'Next page' }).click()
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeVisible()
  await expect(page).toHaveURL(
    new RegExp(`afterAddress=${firstPage.continuation.address.event_sequence}`),
  )
  await page.goBack()

  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeFocused()
  await expect(page).not.toHaveURL(/afterAddress=/)
})

test('restores results focus when browser history changes search scope', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('different scope')
  await search.press('Enter')
  await expect(page).toHaveURL(/q=different(?:\+|%20)scope/)
  await expect(search).toBeFocused()
  await page.goBack()

  await expect(page).toHaveURL(/q=release/)
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeFocused()
})

test('restores focus after a successful search retry', async ({ page }) => {
  await useRecoveringSearchFixture(page)
  await page.goto('/search?q=release')

  const retry = page.getByRole('button', { name: 'Retry' })
  await retry.focus()
  await retry.click()

  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeFocused()
})

test('does not request malformed session or cursor URL state', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto(
    `/search?q=release&session=${'x'.repeat(128)}&afterAddress=${'9'.repeat(128)}&afterProjection=${'9'.repeat(128)}`,
  )

  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  expect(searchRequests).toBe(0)
})

test('does not write malformed search drafts into browser history', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('é'.repeat(257))
  await page.getByRole('textbox', { name: /Exact session/ }).fill('not-a-session')
  await search.press('Enter')

  await expect(page).toHaveURL(/\/search$/)
  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  expect(searchRequests).toBe(0)
})

test('bounds search drafts while they are being edited', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  const session = page.getByRole('textbox', { name: /Exact session/ })
  await search.fill('é'.repeat(bootstrapFixture.limits.max_search_query_bytes))
  await session.fill('x'.repeat(1000))

  await expect(search).toHaveValue('é'.repeat(bootstrapFixture.limits.max_search_query_bytes / 2))
  await expect(session).toHaveValue('x'.repeat(45))
})

test('restores focus to validation after malformed browser history', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release&afterAddress=750')
  await expect(page.getByRole('alert')).toBeVisible()
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await page.goBack()

  await expect(page.getByRole('alert')).toBeFocused()
})

test('does not request NUL-bearing search text', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })

  await page.goto('/search?q=term%00suffix')

  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  expect(searchRequests).toBe(0)
})

test('does not request an unpaired cursor URL field', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto('/search?q=release&afterAddress=750')

  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  expect(searchRequests).toBe(0)
})

test('does not widen repeated exact-session parameters to global search', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto(`/search?q=release&session=${sessionId}&session=${sessionId}`)

  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  expect(searchRequests).toBe(0)
})

test('recovers global search when resubmitting over repeated session parameters', async ({
  page,
}) => {
  await useSearchFixture(page)
  await page.goto(`/search?q=release&session=${sessionId}&session=${sessionId}`)
  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')

  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')

  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await expect(page).not.toHaveURL(/session=/)
})

test('restores focus to validation when history restores repeated session parameters', async ({
  page,
}) => {
  await useSearchFixture(page)
  await page.goto(`/search?q=release&session=${sessionId}&session=${sessionId}`)
  await expect(page.getByRole('alert')).toBeVisible()
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await page.goBack()

  await expect(page.getByRole('alert')).toBeFocused()
})

test('does not search without the bounded JSON capability', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_json: false },
      },
    }),
  )
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto('/search?q=release')

  await expect(
    page.getByRole('heading', {
      name: 'Operational data is not exposed by this daemon contract',
    }),
  ).toBeVisible()
  expect(searchRequests).toBe(0)
})

test('does not expose focusable search fields before capabilities defer Search', async ({
  page,
}) => {
  await page.route('**/api/bootstrap', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 250))
    await route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_lexical_search: false },
      },
    })
  })
  await page.goto('/search')

  const main = page.getByRole('main')
  await main.focus()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveCount(0)
  await expect(
    page.getByRole('heading', {
      name: 'Operational data is not exposed by this daemon contract',
    }),
  ).toBeVisible()
  await expect(main).toBeFocused()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveCount(0)
})

test('refetches when resubmitting the current first-page search', async ({ page }) => {
  await useRefreshingSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()

  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')

  await expect(page.getByText('Refreshing the durable projection.', { exact: true })).toHaveText(
    'Refreshing the durable projection.',
  )
  await expect(page.getByRole('heading', { name: '1 results on this page' })).toBeVisible()
  await expect(page.getByText('1 results loaded on this page.', { exact: true })).toHaveText(
    '1 results loaded on this page.',
  )
  await expect(page.getByText('release planning')).toBeVisible()
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

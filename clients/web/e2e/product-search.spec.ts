import { BROWSER_PREFERENCES_KEY, createDefaultBrowserPreferences } from '../src/preferences'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, type Page, type TestInfo, test } from './fontTest'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
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

// Shared setup reads its expected heading from the fixture, so changing the fixture's cardinality
// never fails other scenarios before they reach their own assertions.
const resultsHeading = (results: readonly unknown[]) =>
  `${results.length} ${results.length === 1 ? 'result' : 'results'}`

const submitSearch = async (page: Page) => {
  await page.getByRole('textbox', { name: 'Search text' }).fill('release evidence')
  await page.getByRole('textbox', { name: /Session ID/ }).fill(sessionId)
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: resultsHeading(firstPage.results) })).toBeVisible()
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
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not announce query validation before bootstrap limits load', async ({ page }) => {
  // Hold bootstrap on an explicit signal rather than a timer: the pending notice below is a
  // positive gate proving the contract has not been admitted yet, so the alert assertion cannot
  // pass by observing the already-successful state.
  let admitBootstrap: () => void = () => undefined
  const bootstrapHeld = new Promise<void>((resolve) => {
    admitBootstrap = resolve
  })
  await page.route('**/api/bootstrap', async (route) => {
    await bootstrapHeld
    await route.fulfill({ json: bootstrapFixture })
  })
  await page.route('**/api/search?**', (route) => route.fulfill({ json: firstPage }))
  await page.goto('/search?q=release')

  await expect(page.getByText('Checking whether bounded search is available…')).toBeVisible()
  await expect(page.getByRole('alert')).toHaveCount(0)

  admitBootstrap()
  await expect(page.getByRole('heading', { name: resultsHeading(firstPage.results) })).toBeVisible()
  await expect(page.getByRole('alert')).toHaveCount(0)
})

test('preserves JSON-shaped lexical text in deep-link URLs', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=%7B%22term%22%3A%22release%22%7D')

  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveValue('{"term":"release"}')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
})

test('replaces the bounded result page through its typed cursor', async ({ page }) => {
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  const nextPage = page.getByRole('button', { name: 'Next' })
  await nextPage.focus()
  await nextPage.click()
  await expect(page.getByRole('heading', { name: '1 result' })).toBeVisible()
  await expect(page).toHaveURL(
    new RegExp(`afterAddress=${firstPage.continuation.address.event_sequence}`),
  )
  await expect(page.getByRole('heading', { name: '1 result' })).toBeFocused()
  await expect(page.getByText('release planning')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus when pagination fails', async ({ page }) => {
  await useFailingPaginationFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  await page.getByRole('button', { name: 'Next' }).click()

  await expect(page.getByRole('heading', { name: 'Search could not be read' })).toBeFocused()
})

test('resets pagination when submitting a different search scope', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  await page.getByRole('button', { name: 'Next' }).click()
  await expect(page.getByRole('heading', { name: '1 result' })).toBeVisible()
  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('different scope')
  await search.press('Enter')

  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
  await expect(page).toHaveURL(/q=different(?:\+|%20)scope/)
  await expect(page).not.toHaveURL(/afterAddress=/)
})

test('synchronizes pagination with browser history', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  await page.getByRole('button', { name: 'Next' }).click()
  await expect(page.getByRole('heading', { name: '1 result' })).toBeVisible()
  await expect(page).toHaveURL(
    new RegExp(`afterAddress=${firstPage.continuation.address.event_sequence}`),
  )
  await page.goBack()

  await expect(page.getByRole('heading', { name: '2 results' })).toBeFocused()
  await expect(page).not.toHaveURL(/afterAddress=/)
})

test('preserves search editing focus when browser history changes scope', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('different scope')
  await search.press('Enter')
  await expect(page).toHaveURL(/q=different(?:\+|%20)scope/)
  await expect(search).toBeFocused()
  await page.goBack()

  await expect(page).toHaveURL(/q=release/)
  await expect(search).toBeFocused()
})

test('restores focus after a successful search retry', async ({ page }) => {
  await useRecoveringSearchFixture(page)
  await page.goto('/search?q=release')

  const retry = page.getByRole('button', { name: 'Retry' })
  await retry.focus()
  await retry.click()

  await expect(page.getByRole('heading', { name: '2 results' })).toBeFocused()
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

  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')
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
  await page.getByRole('textbox', { name: /Session ID/ }).fill('not-a-session')
  await search.press('Enter')

  await expect(page).toHaveURL(/\/search$/)
  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')
  expect(searchRequests).toBe(0)
})

test('bounds search drafts while they are being edited', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  const session = page.getByRole('textbox', { name: /Session ID/ })
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
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
  await page.goBack()

  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
})

test('does not request NUL-bearing search text', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    searchRequests += 1
    return route.fulfill({ json: firstPage })
  })

  await page.goto('/search?q=term%00suffix')

  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')
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

  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')
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

  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')
  expect(searchRequests).toBe(0)
})

test('recovers global search when resubmitting over repeated session parameters', async ({
  page,
}) => {
  await useSearchFixture(page)
  await page.goto(`/search?q=release&session=${sessionId}&session=${sessionId}`)
  await expect(page.getByRole('alert')).toContainText('Invalid search parameters')

  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')

  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
  await expect(page).not.toHaveURL(/session=/)
})

test('restores focus to validation when history restores repeated session parameters', async ({
  page,
}) => {
  await useSearchFixture(page)
  await page.goto(`/search?q=release&session=${sessionId}&session=${sessionId}`)
  await expect(page.getByRole('alert')).toBeVisible()
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
  await page.goBack()

  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
})

// The generated bootstrap decoder requires `bounded_json === true`, so a contract that withholds
// it is rejected before any surface reads it. The optional bounded-lexical-search capability is
// the admitted contract that actually defers Search.
test('does not search without the bounded lexical search capability', async ({ page }) => {
  let searchRequests = 0
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_lexical_search: false },
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
      name: 'This daemon contract does not advertise bounded lexical search',
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
      name: 'This daemon contract does not advertise bounded lexical search',
    }),
  ).toBeVisible()
  await expect(main).toBeFocused()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toHaveCount(0)
})

test('refetches when resubmitting the current first-page search', async ({ page }) => {
  await useRefreshingSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()

  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')

  await expect(page.getByText('Refreshing.', { exact: true })).toHaveText('Refreshing.')
  await expect(page.getByRole('heading', { name: '1 result' })).toBeVisible()
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

  await expect(page.getByRole('alert')).toContainText('Search could not be read')
  await expect(page.getByRole('alert')).toContainText('The Signalbox daemon could not be reached.')
  expect(problems.pageErrors).toEqual([])
})

test('captures desktop dark, desktop light, and responsive search evidence', async ({
  page,
}, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results' })).toBeVisible()
  await expect.soft(page).toHaveScreenshot('search-desktop-dark.png', { animations: 'disabled' })

  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('search-desktop-light.png', { animations: 'disabled' })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible()
  await expect.soft(page).toHaveScreenshot('search-mobile-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('rejects overflowing drafts instead of submitting a valid prefix', async ({ page }) => {
  let requests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/search?**', (route) => {
    requests += 1
    return route.fulfill({ json: firstPage })
  })
  await page.goto('/search')
  const search = page.getByRole('textbox', { name: 'Search text' })
  const session = page.getByRole('textbox', { name: /Exact session/ })
  await search.fill('release')
  await session.fill(`urn:uuid:${sessionId}junk`)
  await search.press('Enter')
  await expect(page.getByRole('alert')).toBeVisible()
  await expect(page).toHaveURL(/\/search$/)
  expect(requests).toBe(0)
  await session.fill('')
  await search.fill('é'.repeat(513))
  await search.press('Enter')
  await expect(page.getByRole('alert')).toBeVisible()
  expect(requests).toBe(0)
})

test('rejects repeated query text without showing an empty search', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=first&q=second')
  await expect(page.getByRole('alert')).toContainText('Search parameters are malformed')
  await expect(
    page.getByRole('heading', { name: 'Search durable text without loading transcripts' }),
  ).toBeHidden()
})

test('clears a draft error when history restores a valid search', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.fill('different')
  await search.press('Enter')
  await expect(page).toHaveURL(/q=different/)
  await page.getByRole('textbox', { name: /Exact session/ }).fill('bad')
  await search.press('Enter')
  await expect(page.getByRole('alert')).toBeVisible()
  await page.goBack()
  await expect(page.getByRole('alert')).toBeHidden()
})

test('withholds previous results after a failed refresh', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search?q=release')
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeVisible()
  await page.route('**/api/search?**', (route) => route.abort())
  await page.getByRole('textbox', { name: 'Search text' }).press('Enter')
  await expect(page.getByRole('heading', { name: 'Search could not be read' })).toBeVisible()
  await expect(page.getByRole('heading', { name: '2 results on this page' })).toBeHidden()
})

test('applies prepaint preferences while the application bundle is pending', async ({ page }) => {
  await useSearchFixture(page)
  await page.addInitScript(
    ({ key, preferences }) => localStorage.setItem(key, JSON.stringify(preferences)),
    {
      key: BROWSER_PREFERENCES_KEY,
      preferences: { ...createDefaultBrowserPreferences(), theme: 'light', density: 'comfortable' },
    },
  )
  const mainReady = Promise.withResolvers<void>()
  await page.route('**/assets/index-*.js', async (route) => {
    await mainReady.promise
    await route.continue()
  })
  try {
    await page.goto('/settings', { waitUntil: 'commit' })
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
    await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
    await expect(page.locator('#root')).toBeEmpty()
    const blockingScript = page.locator('script[blocking~="render"]')
    await expect(blockingScript).toHaveCount(1)
    await expect(blockingScript).toHaveAttribute('src', /\/assets\/prepaint-[^/]+\.js$/)
    await expect(page.locator('script[src*="/assets/index-"]')).not.toHaveAttribute(
      'blocking',
      /render/,
    )
  } finally {
    mainReady.resolve()
  }
  await expect(page.getByRole('heading', { name: 'Operator preferences' })).toBeVisible()
})

test('focuses search through its command and releases editing with Escape', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search')
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeEnabled()
  await page.keyboard.press('Control+Shift+f')
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('main')).toBeFocused()
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Focus lexical search/ }).click()
  await expect(page.getByRole('textbox', { name: 'Search text' })).toBeFocused()
})

test('keeps artifact inspector inputs in their editing context on Search', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/search')
  await page.getByRole('button', { name: 'Open artifact inspector' }).click()
  for (const name of ['Digest', 'Declared media type', 'Display filename optional']) {
    const input = page.getByRole('textbox', { name, exact: true })
    await input.focus()
    await input.press('Escape')
    await expect(input).toBeFocused()
    await expect(page.getByRole('button', { name: 'Close artifact inspector' })).toBeVisible()
  }
})

test('returns palette focus to main when history removes its opener', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/settings')
  await page.getByRole('link', { name: /^Search Global and session search/ }).click()
  await page.getByRole('button', { name: 'Search', exact: true }).focus()
  await page.keyboard.press('ControlOrMeta+k')
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()

  await page.goBack()
  await expect(page).toHaveURL(/\/settings$/)
  await expect(
    page.getByRole('heading', { name: 'Operator preferences', includeHidden: true }),
  ).toBeVisible()
  await expect(palette).toBeVisible()
  await page.keyboard.press('Escape')

  await expect(palette).toBeHidden()
  await expect(page.getByRole('main')).toBeFocused()
})

test('restores surviving product focus when history removes a search control', async ({ page }) => {
  await useSearchFixture(page)
  await page.goto('/settings')
  await page.getByRole('link', { name: /^Search Global and session search/ }).click()
  await page.getByRole('textbox', { name: 'Search text' }).focus()
  await page.goBack()
  await expect(page.getByRole('main')).toBeFocused()
})

test('sets the imports scenario title after leaving a workspace', async ({ page }) => {
  await page.goto('/scenario/streaming')
  await page.getByRole('link', { name: /^Million-row imports/ }).click()
  await expect(page).toHaveTitle('Signalbox Scenario Studio — Imports')
})

test('releases the imports title when the next scenario fails to load', async ({ page }) => {
  await page.route('**/assets/App-*.js', (route) => route.abort())
  await page.goto('/scenario/imports')
  await expect(page).toHaveTitle('Signalbox Scenario Studio — Imports')
  await page.getByRole('link', { name: /^Streaming session/ }).click()
  await expect(page.getByText('Scenario studio could not be loaded.')).toBeVisible()
  await expect(page).not.toHaveTitle('Signalbox Scenario Studio — Imports')
})

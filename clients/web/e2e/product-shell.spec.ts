import { expect, type Page, test } from '@playwright/test'

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

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

const useRecoveringBootstrap = async (page: Page) => {
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => {
    attempts += 1
    return attempts === 1
      ? route.fulfill({ status: 503 })
      : route.fulfill({ json: bootstrapFixture })
  })
}

const watchBrowser = (page: Page) => {
  const problems = { consoleErrors: [] as string[], pageErrors: [] as string[] }
  page.on('console', (message) => {
    if (message.type() === 'error') problems.consoleErrors.push(message.text())
  })
  page.on('pageerror', (error) => problems.pageErrors.push(error.message))
  return problems
}

const platformModifier = (page: Page) =>
  page.evaluate(() => (/Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control'))

const expectedContractStatus = `${bootstrapFixture.contract.name} · ${bootstrapFixture.contract.version}`

test('applies saved visual preferences before the first rendered frame', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem(
      'signalbox.web.preferences.v1',
      JSON.stringify({ theme: 'light', density: 'comfortable' }),
    )
    const observed: string[] = []
    Object.defineProperty(window, '__visualPreferenceMutations', { value: observed })
    requestAnimationFrame(() => {
      observed.push(
        `${document.documentElement.dataset.theme}:${document.documentElement.dataset.density}`,
      )
    })
  })
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window as typeof window & { __visualPreferenceMutations: string[] })
            .__visualPreferenceMutations[0],
      ),
    )
    .toBe('light:comfortable')
})

test('opens the product at Attention with generated-contract transport status', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
  await expect(page.getByText(expectedContractStatus)).toBeVisible()
  await expect(page.getByRole('link', { name: /Attention/ })).toHaveAttribute(
    'aria-current',
    'page',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('navigates from Attention to Sessions with the shared semantic link', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('heading', { name: 'Sessions', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('focuses Scenario studio after cross-route navigation', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Scenario studio/ }).click()

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page.locator('main.workspace')).toBeFocused()
})

test('completes route switching from the command palette without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus after closing the command palette', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const trigger = page.getByRole('button', { name: 'Open command palette' })
  await trigger.click()
  await page.keyboard.press('Escape')
  await expect(trigger).toBeFocused()
})

test('does not offer the palette opener inside the open palette', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  const palette = page.getByRole('dialog', { name: 'Command palette' })

  await expect(palette.getByRole('button', { name: /Open command palette/ })).toHaveCount(0)
})

test('sets route-aware product document titles', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/search')
  await expect(page).toHaveTitle('Search · Signalbox')

  await page.getByRole('link', { name: /Attention/ }).click()
  await expect(page).toHaveTitle('Attention · Signalbox')
})

test('uses a navigation sheet on a phone viewport and unwinds it with Escape', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  const openNavigation = page.getByRole('button', { name: 'Open navigation' })
  await openNavigation.click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(openNavigation).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('closes the phone navigation sheet after route selection', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Sessions/ }).click()
  await expect(navigation).toBeHidden()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores desktop navigation focus to the command-owned surface', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Open navigation/ }).click()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('main')).toBeFocused()
})

test('reports browser-local authority for Settings', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/settings')

  await expect(page.getByText('Browser local', { exact: true })).toBeVisible()
  await expect(page.getByText('Browser storage', { exact: true })).toBeVisible()
  await expect(
    page.getByText('Inspect browser-local workstation presentation preferences.'),
  ).toBeVisible()
  await expect(page.getByText('server-provided evidence', { exact: false })).toHaveCount(0)
  await expect(page.getByText('Bounded query', { exact: true })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Workstation presentation' })).toBeVisible()
  await expect(
    page.getByRole('heading', { name: 'Operational data is not exposed by this daemon contract' }),
  ).toHaveCount(0)
})

test('runs product navigation sequences but leaves Mod+K to an editing field', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/search')

  const search = page.getByRole('textbox', { name: 'Search text' })
  await search.focus()
  await search.evaluate((element) => {
    element.addEventListener('keydown', (event) => {
      queueMicrotask(() => {
        element.dataset.modKDefaultPrevented = String(event.defaultPrevented)
      })
    })
  })
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
  await expect(search).toHaveAttribute('data-mod-k-default-prevented', 'false')
  await search.press('Escape')
  await page.keyboard.press('g')
  await page.keyboard.press('a')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suppresses product navigation sequences while an overlay owns input', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
})

test('suppresses ordinary view hotkeys while an overlay owns input', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.keyboard.press('Shift+D')
  await page.keyboard.press('Shift+T')
  await page.keyboard.press('Shift+W')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'compact')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
  await expect(page.locator('.product-shell')).toHaveClass(/layout-workbench/)
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
})

test('changes visible product spacing with the density control', async ({ page }) => {
  await useDeterministicBootstrap(page)
  await page.goto('/search')

  const surface = page.locator('.surface-body')
  const compactPadding = await surface.evaluate((element) => getComputedStyle(element).paddingTop)
  await page.getByRole('button', { name: 'Use comfortable density' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  const comfortablePadding = await surface.evaluate(
    (element) => getComputedStyle(element).paddingTop,
  )
  expect(comfortablePadding).not.toBe(compactPadding)
})

test('retries an initial bootstrap failure', async ({ page }) => {
  const problems = watchBrowser(page)
  const expectedFailureMessage =
    'Failed to load resource: the server responded with a status of 503 (Service Unavailable)'
  await useRecoveringBootstrap(page)
  await page.goto('/attention')

  await expect(page.getByRole('status')).toContainText('Transport unavailable')
  await page.getByRole('button', { name: 'Retry contract check' }).click()
  await expect(page.getByText(expectedContractStatus)).toBeVisible()
  await expect(page.getByRole('main')).toBeFocused()
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.filter((message) => message !== expectedFailureMessage)).toEqual([])
})

test('distinguishes an incompatible bootstrap contract from an outage', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  await expect(page.getByRole('status')).toContainText('Contract incompatible')
})

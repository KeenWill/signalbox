import { expect, type Page, test } from '@playwright/test'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 65_536 },
} as const

const emptyAttentionFixture = {
  continuation_after_session_id: null,
  cursor: '0',
  summaries: [],
} as const

const useDeterministicProductTransport = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: emptyAttentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => route.fulfill({ json: emptyAttentionFixture }))
}

const useRecoveringBootstrap = async (page: Page) => {
  let attempts = 0
  await page.route('**/api/bootstrap', (route) => {
    attempts += 1
    return attempts === 1
      ? route.fulfill({ status: 503 })
      : route.fulfill({ json: bootstrapFixture })
  })
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

const platformModifier = (page: Page) =>
  page.evaluate(() => (/Mac|iPhone|iPad/.test(navigator.userAgent) ? 'Meta' : 'Control'))

test('opens the product at Attention with generated-contract transport status', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicProductTransport(page)
  await page.goto('/')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
  await expect(page.getByText('signalbox.web-http · 1')).toBeVisible()
  await expect(page.getByRole('link', { name: /Attention/ })).toHaveAttribute(
    'aria-current',
    'page',
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('navigates from Attention to Sessions with the shared semantic link', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('heading', { name: 'Sessions', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('completes route switching from the command palette without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/sessions$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('does not offer the palette-opening command inside the palette', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })

  await expect(palette).toBeVisible()
  await expect(palette.getByRole('button', { name: /Open command palette/ })).toHaveCount(0)
})

test('moves focus to the destination after sequence navigation', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open command palette' }).focus()
  await page.keyboard.press('g')
  await page.keyboard.press('s')

  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('main')).toBeFocused()
})

test('suspends product navigation sequences while the palette owns input', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await palette.getByRole('button', { name: /Go to Sessions/ }).focus()
  await page.keyboard.press('g')
  await page.keyboard.press('s')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(palette).toBeVisible()
})

test('keeps ordinary hotkeys and Escape scoped to the command palette', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await page.keyboard.press('Shift+T')
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
  await page.keyboard.press('Escape')

  await expect(palette).toBeHidden()
  await expect(page.getByRole('heading', { name: 'Attention', level: 1 })).toBeVisible()
})

test('restores desktop navigation focus to the visible palette trigger', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  const paletteTrigger = page.getByRole('button', { name: 'Open command palette' })
  await paletteTrigger.focus()
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: 'Open navigation' }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(paletteTrigger).toBeFocused()
})

test('uses a navigation sheet on a phone viewport and unwinds it with Escape', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicProductTransport(page)
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

test('closes the phone navigation sheet with its semantic close control', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  const openNavigation = page.getByRole('button', { name: 'Open navigation' })
  await openNavigation.click()
  await page.getByRole('button', { name: 'Close product navigation' }).click()

  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(openNavigation).toBeFocused()
})

test('moves focus before focus layout hides the product navigation', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Attention/ }).focus()
  await page.keyboard.press('Shift+W')

  await expect(page.getByRole('main')).toBeFocused()
  await expect(page.getByRole('button', { name: 'Switch to workbench layout' })).toBeVisible()
})

test('closes the phone navigation sheet after route selection', async ({ page }) => {
  await useDeterministicProductTransport(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Sessions/ }).click()

  await expect(page).toHaveURL(/\/sessions$/)
  await expect(navigation).toBeHidden()
})

test('does not start Attention reads when bootstrap validation fails', async ({ page }) => {
  let attentionRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.route('**/api/attention**', (route) => {
    attentionRequests += 1
    return route.abort()
  })

  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention contract unavailable' })).toBeVisible()
  await expect(page.getByText('Contract incompatible')).toBeVisible()
  expect(attentionRequests).toBe(0)
})

test('does not start Attention reads for incompatible bootstrap values', async ({ page }) => {
  let attentionRequests = 0
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: {
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_ndjson_item_bytes: 32_768 },
      },
    }),
  )
  await page.route('**/api/attention**', (route) => {
    attentionRequests += 1
    return route.abort()
  })

  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention contract unavailable' })).toBeVisible()
  await expect(page.getByText('Contract incompatible')).toBeVisible()
  expect(attentionRequests).toBe(0)
})

test('retries a transient bootstrap failure in place', async ({ page }) => {
  await useRecoveringBootstrap(page)
  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention contract unavailable' })).toBeVisible()
  await expect(page.getByText('Transport unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Retry contract check' }).click()

  await expect(page.getByText('signalbox.web-http · 1')).toBeVisible()
  await expect(page.getByRole('heading', { name: '0 sessions' })).toBeVisible()
})

test('gives iconless Attention contract errors the full empty-state width', async ({ page }) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: { invented: true } }))
  await page.goto('/attention')

  const message = page
    .getByRole('heading', { name: 'Attention contract unavailable' })
    .locator('..')
  await expect(message).toHaveCSS('grid-column-start', '1')
  await expect(message).toHaveCSS('grid-column-end', '-1')
})

import { expect, type Page, test } from '@playwright/test'
import {
  richItemPage,
  richRegionPage,
  richSessionId,
  richTimelineWindow,
  richTurnId,
  richTurnPage,
} from './session-detail-fixture'

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

const sessionWorkspaceFixture = {
  id: richSessionId,
  firstAddress: '89',
  latestAddress: '101',
  itemCount: '1000000',
  projectedBytes: richTimelineWindow.projected_structured_bytes,
} as const

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

const useDeterministicSession = async (page: Page) => {
  await page.route('**/api/sessions/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.includes(`/turns/${richTurnId}/`)) {
      return route.fulfill({ json: richTurnPage() })
    }
    if (url.pathname.endsWith('/timeline-detail')) {
      return route.fulfill({ json: richRegionPage(url.searchParams.has('cursor_address')) })
    }
    if (url.pathname.endsWith('/detail')) {
      const address = url.pathname.split('/').at(-2) ?? ''
      return route.fulfill({ json: richItemPage(address) })
    }
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: richTimelineWindow })
    return route.fulfill({
      json: {
        session_id: sessionWorkspaceFixture.id,
        sizes: {
          item_count: sessionWorkspaceFixture.itemCount,
          projected_text_bytes: '0',
          projected_structured_bytes: '96000000',
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: sessionWorkspaceFixture.firstAddress },
        latest_address: { event_sequence: sessionWorkspaceFixture.latestAddress },
        work: { active_turn_count: '1', queued_turn_count: '2' },
        observed_through: sessionWorkspaceFixture.latestAddress,
      },
    })
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

test('opens the product at Attention with generated-contract transport status', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
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
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page.getByRole('heading', { name: 'Sessions', level: 1 })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
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
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
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

test('changes and restores a Settings preference without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto(settingsPreferenceFixture.path)

  const lightTheme = page.getByRole('radio', { name: settingsPreferenceFixture.changedTheme })
  await lightTheme.focus()
  await page.keyboard.press('Space')
  await expect(lightTheme).toBeChecked()
  await page.reload()
  await expect(
    page.getByRole('radio', { name: settingsPreferenceFixture.changedTheme }),
  ).toBeChecked()
  await page.getByRole('button', { name: settingsPreferenceFixture.restoreAction }).focus()
  await page.keyboard.press('Enter')
  await expect(
    page.getByRole('radio', { name: settingsPreferenceFixture.defaultTheme }),
  ).toBeChecked()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses typed item and turn expansion without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(page.getByText(sessionWorkspaceFixture.itemCount, { exact: true })).toBeVisible()
  const failed = page.getByRole('button', { name: /101 turn failed/ })
  await failed.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByText('parked_for_operator_after_ambiguous_effect')).toBeVisible()
  await page.keyboard.press('l')
  await expect(failed).toHaveAttribute('aria-expanded', 'false')
  await page.keyboard.press('l')
  await expect(failed).toHaveAttribute('aria-expanded', 'true')
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('button', { name: /Expand or collapse selected item/ })).toBeVisible()
  await page.keyboard.press('Escape')
  const turn = page.getByRole('button', { name: `Expand turn ${richTurnId}` })
  await turn.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByText('provider_boundary_lost_after_send')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('progressively reads one contiguous region without loading the lifetime corpus', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  const requestedDetails: string[] = []
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  page.on('request', (request) => requestedDetails.push(request.url()))
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(sessionWorkspaceFixture.id)
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await page.getByRole('button', { name: 'Inspect loaded region' }).click()
  await expect(page.getByText('Denied: the release window has closed.')).toBeVisible()
  await page.getByRole('button', { name: 'Continue region' }).click()
  await expect(
    page.getByText('Delegated analysis returned three verified deployment facts.'),
  ).toBeVisible()
  expect(
    requestedDetails.filter((url) => new URL(url).pathname.endsWith('/timeline-detail')),
  ).toHaveLength(2)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

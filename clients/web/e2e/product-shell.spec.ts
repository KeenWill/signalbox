import { expect, type Page, test } from '@playwright/test'

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
    max_ndjson_item_bytes: 65_536,
    max_timeline_detail_items: 128,
    max_timeline_detail_bytes: 65_536,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
  },
} as const

const sessionWorkspaceFixture = {
  id: '00000000-0000-0000-0000-000000000991',
  descriptorFirstAddress: '1',
  firstAddress: '41',
  latestAddress: '43',
  itemCount: '43',
  projectedBytes: 234,
  detail: {
    session_id: '00000000-0000-0000-0000-000000000991',
    items: [
      {
        address: { event_sequence: '43' },
        kind: 'turn_completed',
        body: {
          type: 'turn_lifecycle',
          turn_id: '00000000-0000-0000-0000-000000000043',
          lifecycle: 'terminalized',
          cause_code: 'completed',
        },
        projected_body_bytes: 128,
      },
    ],
    projected_body_bytes: 128,
    continuation: null,
  },
  inputDetail: {
    first: {
      session_id: '00000000-0000-0000-0000-000000000991',
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: {
              text: 'bounded ',
              offset_bytes: '0',
              total_bytes: '14',
              continuation: {
                address: { event_sequence: '41' },
                field: 'input_text',
                member_index: 0,
                offset_bytes: '8',
              },
            },
            attachments: [],
          },
          projected_body_bytes: 136,
        },
      ],
      projected_body_bytes: 136,
      continuation: {
        type: 'more_body',
        body: {
          address: { event_sequence: '41' },
          field: 'input_text',
          member_index: 0,
          offset_bytes: '8',
        },
      },
    },
    second: {
      session_id: '00000000-0000-0000-0000-000000000991',
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: {
              text: 'detail',
              offset_bytes: '8',
              total_bytes: '14',
              continuation: null,
            },
            attachments: [],
          },
          projected_body_bytes: 134,
        },
      ],
      projected_body_bytes: 134,
      continuation: null,
    },
  },
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
    const path = new URL(route.request().url()).pathname
    if (path.endsWith(`/${sessionWorkspaceFixture.firstAddress}/detail`)) {
      const cursor = new URL(route.request().url()).searchParams.get('cursor_offset')
      return route.fulfill({
        json: cursor
          ? sessionWorkspaceFixture.inputDetail.second
          : sessionWorkspaceFixture.inputDetail.first,
      })
    }
    if (path.endsWith(`/${sessionWorkspaceFixture.latestAddress}/detail`)) {
      return route.fulfill({ json: sessionWorkspaceFixture.detail })
    }
    if (path.endsWith('/timeline')) {
      return route.fulfill({
        json: {
          session_id: sessionWorkspaceFixture.id,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '42' },
              kind: 'turn_activated',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '43' },
              kind: 'turn_completed',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: sessionWorkspaceFixture.projectedBytes,
          continuation_before: { event_sequence: sessionWorkspaceFixture.firstAddress },
          continuation_after: null,
        },
      })
    }
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
        first_address: { event_sequence: sessionWorkspaceFixture.descriptorFirstAddress },
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

test('opens and inspects a bounded production session without a mouse', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
  await expect(page.getByText('Active · opened near latest')).toBeVisible()
  await expect(
    page
      .getByLabel('Session telemetry')
      .getByText(sessionWorkspaceFixture.itemCount, { exact: true }),
  ).toBeVisible()
  const completed = page.getByRole('button', {
    name: new RegExp(`${sessionWorkspaceFixture.latestAddress} turn completed`),
  })
  await completed.focus()
  await page.keyboard.press('Enter')
  await expect(completed).toHaveAttribute('aria-expanded', 'true')
  await expect(page.getByText(sessionWorkspaceFixture.detail.items[0].body.lifecycle)).toBeVisible()
  await expect(
    page.getByText(sessionWorkspaceFixture.detail.items[0].body.cause_code, { exact: true }),
  ).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('continues an oversized typed body without retaining an unbounded page', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(sessionWorkspaceFixture.id)
  await page.getByRole('textbox', { name: 'Exact session ID' }).press('Enter')
  const input = page.getByRole('button', {
    name: new RegExp(`${sessionWorkspaceFixture.firstAddress} input accepted`),
  })
  await input.focus()
  await page.keyboard.press('Enter')
  const inputDetail = page.getByRole('region', { name: 'User input' })
  const firstChunk = sessionWorkspaceFixture.inputDetail.first.items[0].body.text.text.trim()
  await expect(inputDetail.getByText(firstChunk, { exact: true })).toBeVisible()
  const continueDetail = page.getByRole('button', { name: 'Load next bounded detail chunk' })
  await continueDetail.focus()
  await page.keyboard.press('Enter')
  await expect(
    inputDetail.getByText(sessionWorkspaceFixture.inputDetail.second.items[0].body.text.text, {
      exact: true,
    }),
  ).toBeVisible()
  await expect(inputDetail.getByText(firstChunk, { exact: true })).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

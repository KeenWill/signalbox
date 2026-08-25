import { expect, type Page, test } from '@playwright/test'

import { webContractBootstrapFixture } from '../src/product.fixture'

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
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: webContractBootstrapFixture }))

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
  await expect(
    page.getByText(
      `${webContractBootstrapFixture.contract.name} · ${webContractBootstrapFixture.contract.version}`,
    ),
  ).toBeVisible()
  await expect(page).toHaveTitle('Attention · Signalbox')
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

test('describes Settings as browser-local rather than daemon-backed', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/settings')

  await expect(
    page.getByRole('heading', { name: 'Local settings are not exposed in this slice' }),
  ).toBeVisible()
  await expect(page.getByText(/do not depend on a daemon read contract/)).toBeVisible()
  await expect(page.getByRole('status')).toHaveText('Browser-local preferences')
  await expect(page.getByText('Transport unavailable', { exact: true })).toHaveCount(0)
  await expect(page.getByText('Incompatible daemon contract', { exact: true })).toHaveCount(0)
  await expect(
    page.getByText('Operational data is not exposed by this daemon contract'),
  ).toHaveCount(0)
  const inspector = page.getByRole('complementary', { name: 'Inspector' })
  await expect(inspector.getByText('Browser', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Local preferences', { exact: true })).toBeVisible()
  await expect(inspector.getByText('Daemon', { exact: true })).toHaveCount(0)
  await expect(
    inspector.getByText('Presentation preferences are stored locally in this browser.'),
  ).toBeVisible()
  await expect(inspector.getByText(/server-provided evidence/)).toHaveCount(0)
  const settingsCopy = page.getByRole('heading', {
    name: 'Local settings are not exposed in this slice',
  })
  expect((await settingsCopy.boundingBox())?.width).toBeGreaterThan(200)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('clears scenario-only help when browser history returns to the product shell', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/\/scenario\/streaming$/)

  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.goBack()

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves a scenario-specific title after leaving the product shell', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const scenarioEntry = page.getByRole('link', { name: /Scenario studio/ })
  await scenarioEntry.focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page).toHaveTitle('Streaming session · Signalbox scenarios')
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeFocused()
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

test('restores focus after opening keyboard help from the command palette', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const openPalette = page.getByRole('button', { name: 'Open command palette' })
  await openPalette.focus()
  await page.keyboard.press('Enter')
  await page.getByRole('button', { name: /Open keyboard help/ }).focus()
  await page.keyboard.press('Enter')
  await page.keyboard.press('Escape')
  await expect(openPalette).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('restores focus after opening keyboard help with the direct hotkey', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const openPalette = page.getByRole('button', { name: 'Open command palette' })
  await openPalette.focus()
  await page.keyboard.press('Shift+/')
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(openPalette).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns focus to a visible desktop control after closing palette-opened navigation', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page.getByRole('button', { name: /Open navigation/ }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('button', { name: 'Open command palette' })).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses the displayed product navigation sequence', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(page).toHaveTitle('Sessions · Signalbox')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus before focus layout hides product navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Sessions/ }).focus()
  await page.keyboard.press('Shift+W')

  await expect(page.getByRole('main')).toBeFocused()
  await expect(page.getByRole('navigation', { name: 'Product' })).toBeHidden()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends product hotkeys while the command palette owns input', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/attention$/)
  await expect(palette).toBeVisible()
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

test('closes phone navigation after selecting a route', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Sessions/ }).click()
  await expect(page).toHaveURL(/\/sessions$/)
  await expect(navigation).toBeHidden()
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
      .getByText('Items', { exact: true })
      .locator('..')
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

test('keeps the window control focused while a different window loads', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(sessionWorkspaceFixture.id)
  await page.getByRole('textbox', { name: 'Exact session ID' }).press('Enter')
  await expect(page.getByText('Active · opened near latest')).toBeVisible()

  let releaseFirstWindow!: () => void
  const firstWindowPending = new Promise<void>((resolve) => {
    releaseFirstWindow = resolve
  })
  await page.route('**/api/sessions/*/timeline?anchor=first*', async (route) => {
    await firstWindowPending
    await route.fulfill({
      json: {
        session_id: sessionWorkspaceFixture.id,
        items: [
          {
            address: { event_sequence: sessionWorkspaceFixture.descriptorFirstAddress },
            kind: 'session_created',
            projected_structured_bytes: 79,
          },
        ],
        projected_structured_bytes: 79,
        continuation_before: null,
        continuation_after: { event_sequence: sessionWorkspaceFixture.descriptorFirstAddress },
      },
    })
  })
  const first = page.getByRole('button', { name: /First/ })
  await first.focus()
  await first.press('Enter')
  await expect(first).toBeFocused()
  releaseFirstWindow()
  await expect(page.getByText('Active · opened at first')).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('normalizes pasted session identity before native validation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicSession(page)
  await page.goto('/sessions')

  const sessionId = page.getByRole('textbox', { name: 'Exact session ID' })
  await sessionId.fill(` ${sessionWorkspaceFixture.id} `)
  await expect(sessionId).toHaveValue(sessionWorkspaceFixture.id)
  await sessionId.press('Enter')
  await expect(page.getByRole('heading', { name: sessionWorkspaceFixture.id })).toBeVisible()
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

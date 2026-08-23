import { expect, type Page, test } from '@playwright/test'
import { useDeterministicImportApi } from './import-api-fixture'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '2' },
  capabilities: {
    bounded_json: true,
    import_discovery: true,
    imported_continuations: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 65_536 },
} as const

const settingsPreferenceFixture = {
  path: '/settings',
  changedTheme: 'Light',
  defaultTheme: 'Dark',
  restoreAction: 'Restore defaults',
} as const

const importsProductFixture = {
  path: '/imports',
  loadedImports: '100',
  latestLoadedPosition: '51',
} as const

const useDeterministicBootstrap = (page: Page) =>
  page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))

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
  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
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
  await expect(page.locator('.product-main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus when browser history changes the product route', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Settings/ }).click()
  await page.getByRole('radio', { name: 'Light' }).focus()

  await page.goBack()

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.locator('.product-main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('preserves focus when focus layout hides the navigation pane', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Sessions/ }).focus()

  await page.keyboard.press('Shift+W')

  await expect(page.locator('.product-shell')).toHaveClass(/layout-focus/)
  await expect(page.locator('.product-main')).toBeFocused()
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
  await expect(page.locator('.product-main')).toBeFocused()
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

test('restores desktop navigation-dialog focus to the visible product main', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await palette.getByRole('button', { name: /Open navigation/ }).click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(page.locator('.product-main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('closes the navigation sheet after selecting a phone route', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation' })
  await navigation.getByRole('link', { name: /Sessions/ }).click()

  await expect(page).toHaveURL(/\/sessions$/)
  await expect(navigation).toBeHidden()

  await page.setViewportSize({ width: 1280, height: 844 })
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await page
    .getByRole('dialog', { name: 'Command palette' })
    .getByRole('button', { name: /Open navigation/ })
    .click()
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.locator('.product-main')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('uses the advertised product navigation sequences', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/sessions$/)
  await page.keyboard.press('g')
  await page.keyboard.press(',')
  await expect(page).toHaveURL(/\/settings$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends product navigation sequences while the palette owns keyboard scope', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  const palette = page.getByRole('dialog', { name: 'Command palette' })
  await expect(palette).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
  await page.keyboard.press('Shift+T')
  await page.keyboard.press('g')
  await page.keyboard.press('s')

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
  await expect(palette).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('enters Scenario studio from the phone drawer without stale focus restoration', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  await page
    .getByRole('dialog', { name: 'Product navigation' })
    .getByRole('link', { name: /Scenario studio/ })
    .click()

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page.getByRole('dialog', { name: 'Product navigation' })).toBeHidden()
  await expect(page.locator('main.workspace')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('moves focus after entering Scenario studio from desktop navigation', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')

  await page.getByRole('link', { name: /Scenario studio/ }).focus()
  await page.keyboard.press('Enter')

  await expect(page).toHaveURL(/\/scenario\/streaming$/)
  await expect(page.locator('main.workspace')).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('clears scenario-only keyboard help when browser history returns to the product', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.goto('/attention')
  await page.getByRole('link', { name: /Scenario studio/ }).click()
  await expect(page).toHaveURL(/\/scenario\/streaming$/)

  const modifier = await platformModifier(page)
  await page.getByRole('button', { name: 'Open command palette' }).click()
  await page.getByRole('button', { name: /Open keyboard help/ }).click()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toBeVisible()
  await page.goBack()

  await expect(page).toHaveURL(/\/attention$/)
  await expect(page.locator('.product-main')).toBeFocused()
  await expect(page.getByRole('dialog', { name: 'Keyboard help' })).toHaveCount(0)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies saved presentation preferences on a direct Imports scenario load', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await page.addInitScript(() => {
    localStorage.setItem(
      'signalbox.web.preferences.v1',
      JSON.stringify({ theme: 'light', density: 'comfortable' }),
    )
  })
  await page.goto('/scenario/imports')

  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
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

test('honors the configured navigation width below the inspector breakpoint', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await page.setViewportSize({ width: 1000, height: 844 })
  await page.goto('/settings')

  await page.getByRole('slider', { name: /^Navigation width/ }).fill('360')

  await expect
    .poll(() =>
      page
        .locator('.product-navigation-pane')
        .evaluate((element) => element.getBoundingClientRect().width),
    )
    .toBe(360)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('operates the bounded Imports surface and leaves through one command palette', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto(importsProductFixture.path)

  const importRows = page.getByRole('rowgroup', { name: 'Imported conversation rows' })
  await expect(importRows).toHaveAttribute('data-total-loaded', importsProductFixture.loadedImports)
  expect(await page.evaluate(() => window.__SIGNALBOX_DIAGNOSTICS__)).toBeUndefined()
  const entries = page.getByRole('listbox', { name: 'Imported source entries' })
  await entries.focus()
  await entries.press('End')
  await expect(entries.getByRole('option', { selected: true })).toHaveAttribute(
    'aria-posinset',
    importsProductFixture.latestLoadedPosition,
  )

  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toHaveCount(1)
  await page.getByRole('button', { name: /Go to Settings/ }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/settings$/)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('withholds Imports until bootstrap admission succeeds', async ({ page }) => {
  const problems = watchBrowser(page)
  let importRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ status: 503 }))
  await page.route('**/api/imports/**', (route) => {
    importRequests += 1
    return route.abort()
  })
  await page.goto('/imports')

  await expect(
    page.getByRole('heading', {
      name: 'Imports are unavailable until bootstrap admission succeeds',
    }),
  ).toBeVisible()
  await expect(page.getByText('Transport unavailable')).toBeVisible()
  const inspector = page.getByRole('complementary', { name: 'Inspector' })
  await expect(inspector.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(inspector.getByText('None', { exact: true })).toBeVisible()
  expect(importRequests).toBe(0)
  expect(problems.pageErrors).toEqual([])
})

test('retries a transient bootstrap transport failure', async ({ page }) => {
  const problems = watchBrowser(page)
  let bootstrapRequests = 0
  await page.route('**/api/bootstrap', (route) => {
    bootstrapRequests += 1
    return bootstrapRequests === 1
      ? route.fulfill({ status: 503 })
      : route.fulfill({ json: bootstrapFixture })
  })
  await useDeterministicImportApi(page)
  await page.goto('/imports')

  await expect(page.getByText('signalbox.web-http · 2')).toBeVisible()
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(bootstrapRequests).toBe(2)
  expect(problems.pageErrors).toEqual([])
})

test('distinguishes contract rejection from transport failure', async ({ page }) => {
  const problems = watchBrowser(page)
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({
      json: { ...bootstrapFixture, contract: { name: 'another.web-http', version: '3' } },
    }),
  )
  await page.goto('/imports')

  await expect(page.getByText('Contract incompatible')).toBeVisible()
  await expect(page.getByText('Transport unavailable')).toHaveCount(0)
  expect(problems.pageErrors).toEqual([])
})

test('serves exact source-session searches through the deterministic adapter', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto('/imports')

  const rows = page.getByRole('rowgroup', { name: 'Imported conversation rows' })
  await expect(rows).toHaveAttribute('data-total-loaded', importsProductFixture.loadedImports)
  await page
    .getByRole('textbox', { name: 'Filter imports by exact source session evidence' })
    .fill('source-session-0')
  await page.getByRole('checkbox', { name: 'Use exact source session filter' }).check()

  await expect(rows).toHaveAttribute('data-total-loaded', '1')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies product density to Imports virtual rows', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto('/imports')

  const row = page
    .getByRole('rowgroup', { name: 'Imported conversation rows' })
    .getByRole('row')
    .first()
  await expect(row).toBeVisible()
  const compactHeight = await row.evaluate((element) => element.getBoundingClientRect().height)
  await page.getByRole('button', { name: 'Use comfortable density' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable')
  await expect
    .poll(() => row.evaluate((element) => element.getBoundingClientRect().height))
    .toBeGreaterThan(compactHeight)
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('suspends Imports hotkeys while the palette owns keyboard scope', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.goto('/imports')

  const entries = page.locator('[aria-label="Imported source entries"]')
  await entries.focus()
  const initialSelection = await entries.getAttribute('aria-activedescendant')
  expect(initialSelection).not.toBeNull()
  const modifier = await platformModifier(page)
  await page.keyboard.press(`${modifier}+K`)
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('j')
  await expect(entries).toHaveAttribute('aria-activedescendant', initialSelection ?? '')
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('scrolls short Imports workbenches instead of clipping the inspector', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.setViewportSize({ width: 1200, height: 560 })
  await page.goto('/imports')

  const main = page.locator('.product-main-imports')
  await expect(page.getByRole('rowgroup', { name: 'Imported conversation rows' })).toBeVisible()
  expect(await main.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true)
  await main.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(page.getByRole('heading', { name: 'Import inspector' })).toBeVisible()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('switches Imports layout before the product pane clips', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.setViewportSize({ width: 920, height: 844 })
  await page.goto('/imports')

  const workspace = page.locator('.imports-workspace-product')
  await expect(workspace).toBeVisible()
  expect(await workspace.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
    true,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('stacks Imports from the available product pane width', async ({ page }) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.addInitScript(() => {
    localStorage.setItem(
      'signalbox.web.preferences.v1',
      JSON.stringify({ paneSizes: { navigation: 360, inspector: 480 } }),
    )
  })
  await page.setViewportSize({ width: 1280, height: 844 })
  await page.goto('/imports')

  const workspace = page.locator('.imports-workspace-product')
  const inspectorBody = page.locator('.import-inspector-body')
  await expect(workspace).toBeVisible()
  await expect(inspectorBody).toHaveCSS('grid-template-columns', /^(?!.* ).+$/)
  expect(await workspace.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
    true,
  )
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('locks product navigation while an ambiguous continuation command is retained', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await useDeterministicBootstrap(page)
  await useDeterministicImportApi(page)
  await page.route('**/api/imports/*/continuations', (route) =>
    route.fulfill({
      status: 503,
      json: {
        error: {
          kind: 'application',
          code: 'continuation_commit_ambiguous',
          message: 'The commit outcome is ambiguous.',
        },
      },
    }),
  )
  await page.goto('/imports')

  await page
    .getByRole('textbox', { name: 'Initial model selection UUID' })
    .fill('00000000-0000-7000-8000-000000000777')
  await page.getByRole('button', { name: 'Resume' }).click()
  await expect(page.getByRole('button', { name: 'Retry exact command' })).toBeVisible()

  const settingsLink = page.getByRole('link', { name: /Settings/ })
  await expect(settingsLink).toHaveAttribute('aria-disabled', 'true')
  await settingsLink.click({ force: true })
  await expect(page).toHaveURL(/\/imports$/)
  await page.keyboard.press('g')
  await page.keyboard.press('s')
  await expect(page).toHaveURL(/\/imports$/)
  await expect(page.getByRole('button', { name: 'Retry exact command' })).toBeVisible()
  const expectedResourceError =
    'Failed to load resource: the server responded with a status of 503 (Service Unavailable)'
  expect(problems.pageErrors).toEqual([])
  expect(problems.consoleErrors.length).toBeLessThanOrEqual(1)
  expect(problems.consoleErrors.every((error) => error === expectedResourceError)).toBe(true)
})

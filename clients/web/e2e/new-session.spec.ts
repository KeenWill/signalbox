import { templateFixture } from '../src/features/templates/fixtures'
import { createdSessionFixture } from '../src/newSession.fixture'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'

const setup = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/templates', (route) =>
    route.fulfill({ json: { templates: [templateFixture] } }),
  )
}

test('creates a session from the template picker with the keyboard', async ({ page }, testInfo) => {
  await setup(page)
  const requests: unknown[] = []
  await page.route('**/api/sessions', async (route) => {
    requests.push(route.request().postDataJSON())
    await route.fulfill({ status: 201, json: createdSessionFixture })
  })
  await page.goto('/settings')
  const opener = page
    .getByRole('navigation', { name: 'Product' })
    .getByRole('button', { name: 'New session', exact: true })
  await opener.focus()
  await page.keyboard.press('Enter')
  const dialog = page.getByRole('dialog', { name: 'New session', exact: true })
  await expect(dialog.getByLabel('Template')).toHaveValue(templateFixture.name)
  await dialog.getByLabel('Template').focus()
  await page.keyboard.press('Tab')
  await expect(dialog.getByRole('button', { name: 'Create session', exact: true })).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('new-session-picker.png') })
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(
    new RegExp(`/sessions\\?.*session=${createdSessionFixture.session_id}`),
  )
  await expect(dialog).toBeHidden()
  await expect(
    page.getByRole('main').getByRole('button', { name: 'New session', exact: true }),
  ).toHaveCount(0)
  expect(requests).toEqual([
    { command_id: expect.any(String), template_name: templateFixture.name, first_input: null },
  ])
})

test('retries the same creation after a lost response and reload', async ({ page }) => {
  await setup(page)
  const requests: unknown[] = []
  await page.route('**/api/sessions', async (route) => {
    requests.push(route.request().postDataJSON())
    if (requests.length === 1) await route.abort()
    else await route.fulfill({ status: 201, json: createdSessionFixture })
  })
  await page.goto('/settings')
  await page.getByRole('button', { name: 'New session', exact: true }).click()
  await page.getByRole('button', { name: 'Create session', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Retry creation', exact: true })).toBeEnabled()
  await page.reload()
  await page.getByRole('button', { name: 'New session', exact: true }).click()
  await page.getByRole('button', { name: 'Retry creation', exact: true }).click()
  await expect(page).toHaveURL(new RegExp(`session=${createdSessionFixture.session_id}`))
  expect(requests).toHaveLength(2)
  expect(requests[1]).toEqual(requests[0])
  await expect
    .poll(() => page.evaluate(() => sessionStorage.getItem('signalbox.new-session')))
    .toBeNull()
})

test('shows template load failure and an empty catalog without posting', async ({ page }) => {
  await setup(page)
  let available = false
  await page.route('**/api/templates', (route) =>
    route.fulfill(
      available
        ? { json: { templates: [] } }
        : {
            status: 503,
            json: {
              error: {
                kind: 'application',
                code: 'templates_unavailable',
                message: 'Templates unavailable',
              },
            },
          },
    ),
  )
  await page.goto('/settings')
  await page.getByRole('button', { name: 'New session', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Retry templates', exact: true })).toBeVisible()
  available = true
  await page.getByRole('button', { name: 'Retry templates', exact: true }).click()
  await expect(page.getByLabel('Template')).toContainText('No templates available')
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeDisabled()
})

test('offers New session in the palette and restores focus when cancelled on a phone', async ({
  page,
}, testInfo) => {
  await setup(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/settings')
  const palette = page.getByRole('button', { name: 'Open command palette', exact: true })
  await palette.click()
  await page
    .getByRole('dialog', { name: 'Command palette' })
    .getByRole('button', { name: /^New session/ })
    .click()
  await expect(page.getByRole('dialog', { name: 'New session', exact: true })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('new-session-phone.png') })
  await page.keyboard.press('Escape')
  await expect(palette).toBeFocused()
})

test('opens creation from the sidebar rail and phone navigation with focus recovery', async ({
  page,
}, testInfo) => {
  await setup(page)
  await page.goto('/settings')
  await page.getByRole('button', { name: 'Collapse sidebar' }).click()
  const railOpener = page
    .getByRole('navigation', { name: 'Product' })
    .getByRole('button', { name: 'New session', exact: true })
  await railOpener.click()
  const dialog = page.getByRole('dialog', { name: 'New session', exact: true })
  await expect(dialog.getByLabel('Template')).toHaveValue(templateFixture.name)
  await page.keyboard.press('Escape')
  await expect(railOpener).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('new-session-navigation-rail.png') })
  await page.setViewportSize({ width: 390, height: 844 })
  const navigationOpener = page.getByRole('button', { name: 'Open navigation', exact: true })
  await navigationOpener.click()
  const navigation = page.getByRole('dialog', { name: 'Product navigation', exact: true })
  await page.screenshot({ path: testInfo.outputPath('new-session-navigation-phone.png') })
  await navigation.getByRole('button', { name: 'New session', exact: true }).click()
  await expect(navigation).toBeHidden()
  await expect(dialog.getByLabel('Template')).toHaveValue(templateFixture.name)
  await expect(dialog.getByRole('button', { name: 'Close new session' })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(navigationOpener).toBeFocused()
})

test('releases a timed-out creation for cancellation and retries the retained request', async ({
  page,
}) => {
  await setup(page)
  await page.clock.install()
  const requests: unknown[] = []
  let release = () => {}
  const stalled = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/sessions', async (route) => {
    requests.push(route.request().postDataJSON())
    if (requests.length === 1) {
      await stalled
      return route.abort()
    }
    return route.fulfill({ status: 201, json: createdSessionFixture })
  })
  try {
    await page.goto('/settings')
    const opener = page.getByRole('button', { name: 'New session', exact: true })
    await opener.click()
    await page.getByRole('button', { name: 'Create session', exact: true }).click()
    await expect.poll(() => requests.length).toBe(1)
    await expect(page.getByRole('button', { name: 'Close new session' })).toBeDisabled()
    await page.clock.fastForward(30_001)
    await expect(page.getByRole('button', { name: 'Retry creation', exact: true })).toBeEnabled()
    await expect(page.getByRole('button', { name: 'Close new session' })).toBeEnabled()
    await page.keyboard.press('Escape')
    await expect(opener).toBeFocused()
    await opener.click()
    await page.getByRole('button', { name: 'Retry creation', exact: true }).click()
    await expect(page).toHaveURL(new RegExp(`session=${createdSessionFixture.session_id}`))
    expect(requests).toHaveLength(2)
    expect(requests[1]).toEqual(requests[0])
  } finally {
    release()
  }
})

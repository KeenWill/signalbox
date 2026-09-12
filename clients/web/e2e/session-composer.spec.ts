import { expect, test } from './fontTest'
import { openSession, sessionApi, sessionId } from './session-fixture'

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`chat composer ${viewport.name}`, async ({ page }, testInfo) => {
    await page.setViewportSize(viewport)
    const api = await sessionApi(page)
    await openSession(page)
    const message = page.getByRole('textbox', { name: 'Message', exact: true })
    await expect(page.getByText('Write a message to send', { exact: true })).toBeVisible()
    await expect(
      page.getByRole('button', { name: 'Attach files (unavailable)', exact: true }),
    ).toBeDisabled()
    await message.fill('First line')
    await message.press('Shift+Enter')
    await message.press('x')
    await expect(message).toHaveValue('First line\nx')
    expect(api.state.submissions).toHaveLength(0)
    await page.screenshot({
      path: testInfo.outputPath(`composer-${viewport.name}.png`),
      fullPage: true,
    })
    await message.press('Enter')
    await expect(message).toHaveValue('')
    expect(api.state.submissions).toHaveLength(1)
    expect(api.state.submissions[0]?.message).toBe('First line\nx')
  })
}
test('active work prevents sending a draft', async ({ page }) => {
  const api = await sessionApi(page, true)
  await openSession(page)
  const message = page.getByRole('textbox', { name: 'Message', exact: true })
  await message.fill('A draft')
  await expect(
    page.getByText('Wait for the current turn to finish · Running', { exact: true }),
  ).toBeVisible()
  await message.press('Enter')
  expect(api.state.submissions).toHaveLength(0)
  await expect(message).toHaveValue('A draft')
})
test('IME confirmation does not submit', async ({ page }) => {
  const api = await sessionApi(page)
  await openSession(page)
  const message = page.getByRole('textbox', { name: 'Message', exact: true })
  await message.fill('Composing')
  await message.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true })
  expect(api.state.submissions).toHaveLength(0)
  await expect(message).toHaveValue('Composing')
})

test('rejection remains visible when another turn starts', async ({ page }) => {
  const api = await sessionApi(page)
  await page.route(`**/api/sessions/${sessionId}/input`, async (route) => {
    api.state.active = true
    api.grow()
    await expect(page.getByRole('paragraph').filter({ hasText: /^Active$/ })).toBeVisible()
    await route.fulfill({
      status: 409,
      json: {
        error: {
          kind: 'application',
          code: 'active_turn_present',
          message: 'Another turn started first',
        },
      },
    })
  })
  await openSession(page)
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill('Keep this draft')
  await page.getByRole('button', { name: 'Send message', exact: true }).click()
  const status = page.getByRole('form', { name: 'Message composer' }).getByRole('status')
  await expect(status).toContainText('Message rejected: Another turn started first')
  await expect(status).toContainText('Wait for the current turn to finish · Running')
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue(
    'Keep this draft',
  )
})

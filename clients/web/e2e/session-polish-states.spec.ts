import { expect, test } from './fontTest'
import { initialMessage, sessionApi, sessionId } from './session-fixture'

for (const width of [1440, 390]) {
  test(`session load failure keeps a retry beside the message at ${width}`, async ({
    page,
  }, testInfo) => {
    await page.setViewportSize({ width, height: 900 })
    await sessionApi(page)
    const descriptor = `**/api/sessions/${sessionId}`
    await page.route(descriptor, (route) =>
      route.fulfill({ status: 503, json: { error: 'Unavailable' } }),
    )
    await page.goto(`/sessions?workspace=true&session=${sessionId}`)
    await expect(page.getByRole('alert')).toContainText('Session failed to load.')
    await page.getByRole('textbox', { name: 'Message', exact: true }).fill('Keep this draft')
    const retry = page.getByRole('button', { name: 'Retry session', exact: true })
    await expect(retry).toBeInViewport()
    await retry.focus()
    await page.screenshot({ path: testInfo.outputPath(`session-error-${width}.png`) })
    const workspace = page.getByRole('region', { name: 'Session workspace', exact: true })
    await retry.press('Enter')
    await expect(workspace).toBeFocused()
    await expect(retry).toBeEnabled()
    await expect(workspace).toBeFocused()
    await retry.focus()
    await page.unroute(descriptor)
    await retry.press('Enter')
    await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
    await expect(workspace).toBeFocused()
    await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue(
      'Keep this draft',
    )
  })
}

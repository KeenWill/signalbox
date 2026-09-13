import { expect, test } from './fontTest'
import { openSession, sessionApi, sessionId, turnId } from './session-fixture'

const requestId = '20000000-0000-4000-8000-000000000001'
const goalStatement = '  Finish the review\n'
const successorMessage = '  Focus on the parser\n'

test('approving a pending request confirms once inline', async ({ page }, testInfo) => {
  const api = await sessionApi(page, true)
  api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/approvals/${requestId}`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Approve', exact: true }).click()
  expect(requests).toHaveLength(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('approval-confirmation.png'), fullPage: true })
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([{ command_id: expect.any(String), decision: 'approve', note: null }])
})

test('a denial shows the daemon rejection', async ({ page }) => {
  const api = await sessionApi(page, true)
  api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
  await page.route(`**/api/sessions/${sessionId}/approvals/${requestId}`, (route) =>
    route.fulfill({
      status: 409,
      json: {
        error: {
          kind: 'application',
          code: 'awaiting_approval_judge',
          message: 'The approval judge must finish first.',
        },
      },
    }),
  )
  await openSession(page)
  await page.getByRole('button', { name: 'Deny', exact: true }).click()
  await page.getByRole('textbox', { name: 'Note (optional)' }).fill('Outside the requested work')
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('alert')).toContainText(
    'awaiting_approval_judge: The approval judge must finish first.',
  )
})

test('goal retry keeps its original command and statement', async ({ page }) => {
  await sessionApi(page)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/goal`, (route) => {
    requests.push(route.request().postDataJSON())
    return requests.length === 1 ? route.abort('failed') : route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Set goal', exact: true }).click()
  await page.getByRole('textbox', { name: 'Goal', exact: true }).fill(goalStatement)
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Goal', exact: true })).toBeDisabled()
  await page.getByRole('button', { name: 'Retry same action' }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toHaveLength(2)
  expect(requests[0]).toEqual({ command_id: expect.any(String), statement: goalStatement })
  expect(requests[1]).toEqual(requests[0])
})

test('cancel retains its successor and identity across navigation', async ({ page }, testInfo) => {
  await sessionApi(page, true)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/cancel`, (route) => {
    requests.push(route.request().postDataJSON())
    return requests.length === 1 ? route.abort('failed') : route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Cancel turn', exact: true }).click()
  expect(requests).toHaveLength(0)
  await expect(page.getByRole('button', { name: 'Confirm', exact: true })).toBeDisabled()
  await page.getByRole('textbox', { name: 'Message to continue with' }).fill(successorMessage)
  await page.screenshot({ path: testInfo.outputPath('cancel-confirmation.png'), fullPage: true })
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Message to continue with' })).toBeDisabled()
  await expect(page.getByRole('alert')).toContainText('Outcome unconfirmed')
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await expect(page).toHaveURL(/\/settings$/)
  await page.goBack()
  await expect(page.getByRole('textbox', { name: 'Message to continue with' })).toHaveValue(
    successorMessage,
  )
  await page.getByRole('button', { name: 'Retry same action' }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([
    {
      command_id: expect.any(String),
      expected_active_turn_id: turnId,
      message: successorMessage,
    },
    requests[0],
  ])
})

test('an idle session does not offer cancel', async ({ page }) => {
  await sessionApi(page)
  await openSession(page)
  await expect(page.getByRole('button', { name: 'Set goal', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Cancel turn', exact: true })).toHaveCount(0)
})

test('two synchronous confirmations send one command', async ({ page }) => {
  await sessionApi(page, true)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/cancel`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Cancel turn', exact: true }).click()
  await page.getByRole('textbox', { name: 'Message to continue with' }).fill(successorMessage)
  await page.getByRole('button', { name: 'Confirm', exact: true }).evaluate((button) => {
    if (!(button instanceof HTMLButtonElement)) throw new Error('Confirm must be a button')
    button.click()
    button.click()
  })
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([
    { command_id: expect.any(String), expected_active_turn_id: turnId, message: successorMessage },
  ])
})

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
  { name: 'minimum phone', width: 320, height: 844 },
]) {
  test(`all session actions fit the ${viewport.name} header`, async ({ page }, testInfo) => {
    await page.setViewportSize(viewport)
    const api = await sessionApi(page, true)
    api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    await openSession(page)
    const header = page.locator('.session-compact-header')
    await expect(header.locator('.session-header-line')).toHaveCount(2)
    const box = await header.boundingBox()
    if (viewport.width >= 390) expect(box?.height).toBeLessThanOrEqual(70)
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(
      viewport.width,
    )
    for (const name of ['Approve', 'Deny', 'Cancel turn', 'Set goal', 'Clear goal']) {
      const control = header.getByRole('button', { name, exact: true })
      await expect(control).toBeVisible()
      const bounds = await control.boundingBox()
      expect(bounds?.x).toBeGreaterThanOrEqual(box?.x ?? 0)
      expect((bounds?.x ?? 0) + (bounds?.width ?? 0)).toBeLessThanOrEqual(
        (box?.x ?? 0) + (box?.width ?? 0),
      )
    }
    const details = await header.getByText('Session details', { exact: true }).boundingBox()
    expect((details?.x ?? 0) + (details?.width ?? 0)).toBeLessThanOrEqual(
      (box?.x ?? 0) + (box?.width ?? 0),
    )
    await page.screenshot({
      path: testInfo.outputPath(`actions-header-${viewport.name}.png`),
      fullPage: true,
    })
    await header.getByRole('button', { name: 'Deny', exact: true }).click()
    await expect(page.getByRole('textbox', { name: 'Note (optional)' })).toBeVisible()
    await expect(header.getByRole('button', { name: 'Confirm', exact: true })).toHaveCount(0)
    if (viewport.width >= 390) expect((await header.boundingBox())?.height).toBeLessThanOrEqual(70)
  })
}

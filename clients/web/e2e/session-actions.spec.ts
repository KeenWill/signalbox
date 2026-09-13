import { expect, test } from './fontTest'
import { openSession, sessionApi, sessionId, turnId } from './session-fixture'

const requestId = '20000000-0000-4000-8000-000000000001'

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
  await page.getByRole('textbox', { name: 'Goal', exact: true }).fill('Finish the review')
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Goal', exact: true })).toBeDisabled()
  await page.getByRole('button', { name: 'Retry same action' }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toHaveLength(2)
  expect(requests[1]).toEqual(requests[0])
})

test('cancel requires a successor message and retries the same turn', async ({
  page,
}, testInfo) => {
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
  await page.getByRole('textbox', { name: 'Message to continue with' }).fill('Focus on the parser')
  await page.screenshot({ path: testInfo.outputPath('cancel-confirmation.png'), fullPage: true })
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Message to continue with' })).toBeDisabled()
  await page.getByRole('button', { name: 'Retry same action' }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([
    {
      command_id: expect.any(String),
      expected_active_turn_id: turnId,
      message: 'Focus on the parser',
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

import { expect, test } from './fontTest'
import {
  openSession,
  openSessionFromCatalog,
  sessionApi,
  sessionId,
  turnId,
} from './session-fixture'

const requestId = '20000000-0000-4000-8000-000000000001'
const goalStatement = '  Finish the review\n'
const successorMessage = '  Focus on the parser\n'
const nextTurnId = '00000000-0000-0000-0000-000000000993'

test('approving a pending request confirms once inline', async ({ page }, testInfo) => {
  const api = await sessionApi(page, true)
  api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/approvals/${requestId}`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await expect(page.getByRole('button', { name: 'Cancel turn', exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Clear goal', exact: true })).toHaveCount(0)
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

for (const denial of [
  {
    name: 'preserves NBSP edges',
    text: '\u00a0Outside the requested work\u00a0',
    note: '\u00a0Outside the requested work\u00a0',
  },
  { name: 'preserves NBSP only', text: '\u00a0', note: '\u00a0' },
  { name: 'omits POSIX whitespace only', text: ' \t\n\v\f', note: null },
  { name: 'strips ordinary trailing whitespace', text: 'Outside scope ', note: 'Outside scope' },
  {
    name: 'strips POSIX edges and preserves interior lines',
    text: ' \t\n\v\f\rFirst line\n  Second line\r\f\v\n\t ',
    note: 'First line\n  Second line',
  },
  {
    name: 'preserves NBSP inside POSIX edges',
    text: ' \t\u00a0Outside scope\u00a0\r\n ',
    note: '\u00a0Outside scope\u00a0',
  },
]) {
  test(`denial submission ${denial.name}`, async ({ page }) => {
    const api = await sessionApi(page, true)
    api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    const requests: unknown[] = []
    await page.route(`**/api/sessions/${sessionId}/approvals/${requestId}`, (route) => {
      requests.push(route.request().postDataJSON())
      return route.fulfill({ status: 204 })
    })
    await openSession(page)
    await page.getByRole('button', { name: 'Deny', exact: true }).click()
    await page.getByRole('textbox', { name: 'Note (optional)' }).fill(denial.text)
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
    expect(requests).toEqual([
      { command_id: expect.any(String), decision: 'deny', note: denial.note },
    ])
  })
}

test('goal retry keeps its original command and statement without randomUUID', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(crypto, 'randomUUID', { value: undefined })
  })
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
  expect(requests[0]).toEqual({
    command_id: expect.stringMatching(
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    ),
    statement: goalStatement,
  })
  expect(requests[1]).toEqual(requests[0])
})

test('a whitespace-only goal is submitted exactly', async ({ page }) => {
  await sessionApi(page)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/goal`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Set goal', exact: true }).click()
  await page.getByRole('textbox', { name: 'Goal', exact: true }).fill(' \n')
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([{ command_id: expect.any(String), statement: ' \n' }])
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

test('a whitespace-only cancellation successor is submitted exactly', async ({ page }) => {
  await sessionApi(page, true)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/cancel`, (route) => {
    requests.push(route.request().postDataJSON())
    return route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Cancel turn', exact: true }).click()
  await page.getByRole('textbox', { name: 'Message to continue with' }).fill(' \n')
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([
    { command_id: expect.any(String), expected_active_turn_id: turnId, message: ' \n' },
  ])
})

for (const action of [
  {
    button: 'Set goal',
    field: 'Goal',
    path: 'goal',
    contentKey: 'statement',
    payload: { statement: goalStatement },
  },
  {
    button: 'Cancel turn',
    field: 'Message to continue with',
    path: 'cancel',
    contentKey: 'message',
    payload: { message: successorMessage, expected_active_turn_id: turnId },
  },
  {
    button: 'Deny',
    field: 'Note (optional)',
    path: `approvals/${requestId}`,
    contentKey: 'note',
    payload: { note: '\u00a0Outside the requested work\u00a0', decision: 'deny' },
  },
]) {
  test(`a rejected ${action.button} draft survives settlement after unmount`, async ({ page }) => {
    const api = await sessionApi(page, true)
    if (action.button === 'Deny') {
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    }
    let release = () => {}
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    const requests: Record<string, unknown>[] = []
    await page.route(`**/api/sessions/${sessionId}/${action.path}`, async (route) => {
      requests.push(route.request().postDataJSON())
      if (requests.length === 1) {
        await held
        return route.fulfill({
          status: 409,
          json: {
            error: {
              kind: 'application',
              code: 'action_refused',
              message: 'The action was refused.',
            },
          },
        })
      }
      return route.fulfill({ status: 204 })
    })
    await openSession(page)
    await page.getByRole('button', { name: action.button, exact: true }).click()
    const field = page.getByRole('textbox', { name: action.field, exact: true })
    const originalText = action.payload[action.contentKey as keyof typeof action.payload] as string
    await field.fill(originalText)
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect.poll(() => requests.length).toBe(1)
    await page.getByRole('link', { name: 'Settings', exact: true }).click()
    await expect(page).toHaveURL(/\/settings$/)
    const refused = page.waitForResponse((response) => response.status() === 409)
    release()
    await refused
    await page.goBack()
    await expect(page.getByRole('alert')).toContainText('action_refused: The action was refused.')
    await expect(field).toHaveValue(originalText)
    await expect(field).toBeEnabled()
    await page.getByRole('link', { name: 'Settings', exact: true }).click()
    await expect(page).toHaveURL(/\/settings$/)
    await page.goBack()
    await expect(field).toHaveValue(originalText)
    const revisedText = 'Revised after returning'
    await field.fill(revisedText)
    await page.getByRole('link', { name: 'Settings', exact: true }).click()
    await expect(page).toHaveURL(/\/settings$/)
    await page.goBack()
    await expect(field).toHaveValue(revisedText)
    await expect(field).toBeEnabled()
    await expect(page.getByRole('alert')).toContainText('action_refused: The action was refused.')
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
    await expect(page.getByRole('region', { name: 'Session workspace', exact: true })).toBeFocused()
    expect(requests).toHaveLength(2)
    expect(requests[1]).toEqual({
      ...action.payload,
      [action.contentKey]: revisedText,
      command_id: expect.any(String),
    })
    expect(requests[1]?.command_id).not.toBe(requests[0]?.command_id)
    await page.getByRole('link', { name: 'Settings', exact: true }).click()
    await expect(page).toHaveURL(/\/settings$/)
    await page.goBack()
    await expect(field).toHaveCount(0)
  })

  test(`a rejected ${action.button} retry restores the editable draft after navigation`, async ({
    page,
  }) => {
    const api = await sessionApi(page, true)
    if (action.button === 'Deny') {
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    }
    const requests: Record<string, unknown>[] = []
    await page.route(`**/api/sessions/${sessionId}/${action.path}`, (route) => {
      requests.push(route.request().postDataJSON())
      if (requests.length === 1) return route.abort('failed')
      if (requests.length === 2)
        return route.fulfill({
          status: 409,
          json: {
            error: {
              kind: 'application',
              code: 'action_refused',
              message: 'The action was refused.',
            },
          },
        })
      return route.fulfill({ status: 204 })
    })
    await openSession(page)
    await page.getByRole('button', { name: action.button, exact: true }).click()
    const field = page.getByRole('textbox', { name: action.field, exact: true })
    const originalText = action.payload[action.contentKey as keyof typeof action.payload] as string
    await field.fill(originalText)
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect(page.getByRole('alert')).toContainText('Outcome unconfirmed')
    await page.getByRole('link', { name: 'Settings', exact: true }).click()
    await expect(page).toHaveURL(/\/settings$/)
    await page.goBack()
    await expect(field).toHaveValue(originalText)
    await expect(field).toBeDisabled()
    await page.getByRole('button', { name: 'Retry same action' }).click()
    await expect(page.getByRole('alert')).toContainText('action_refused: The action was refused.')
    expect(requests).toEqual([{ command_id: expect.any(String), ...action.payload }, requests[0]])
    await expect(field).toHaveValue(originalText)
    await expect(field).toBeEnabled()
    const revisedText = 'Revised request'
    await field.fill(revisedText)
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
    expect(requests).toHaveLength(3)
    expect(requests[2]).toEqual({
      ...action.payload,
      [action.contentKey]: revisedText,
      command_id: expect.any(String),
    })
    expect(requests[2]?.command_id).not.toBe(requests[0]?.command_id)
  })
}

test('an idle session does not offer cancel', async ({ page }) => {
  await sessionApi(page)
  await openSession(page)
  await expect(page.getByRole('button', { name: 'Set goal', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Clear goal', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Cancel turn', exact: true })).toHaveCount(0)
})

for (const resolution of ['Escape', 'Keep unchanged']) {
  test(`recovered drafts retain capacity until ${resolution}`, async ({ page }) => {
    // Four retained actions fill the existing capacity; the fifth session must wait.
    const sessions = [
      sessionId,
      ...Array.from(
        { length: 4 },
        (_, index) => `00000000-0000-4000-8000-${String(index + 1).padStart(12, '0')}`,
      ),
    ]
    for (const id of sessions) await sessionApi(page, false, id)
    await openSession(page)
    for (const [index, id] of sessions.slice(0, 4).entries()) {
      let release = () => {}
      const held = new Promise<void>((resolve) => {
        release = resolve
      })
      await page.route(`**/api/sessions/${id}/goal`, async (route) => {
        await held
        return route.fulfill({
          status: 409,
          json: {
            error: {
              kind: 'application',
              code: 'action_refused',
              message: 'The action was refused.',
            },
          },
        })
      })
      await page.getByRole('button', { name: 'Set goal', exact: true }).click()
      await page.getByRole('textbox', { name: 'Goal', exact: true }).fill(goalStatement)
      await page.getByRole('button', { name: 'Confirm', exact: true }).click()
      await expect(page.getByRole('button', { name: 'Sending…', exact: true })).toBeDisabled()
      await page.getByRole('link', { name: 'Settings', exact: true }).click()
      await expect(page).toHaveURL(/\/settings$/)
      const refused = page.waitForResponse(
        (response) => response.url().endsWith(`${id}/goal`) && response.status() === 409,
      )
      release()
      await refused
      const next = sessions[index + 1]
      if (!next) throw new Error('Missing next session fixture')
      await openSessionFromCatalog(page, next)
    }
    await expect(page.getByRole('button', { name: 'Set goal', exact: true })).toBeDisabled()
    await expect(
      page.getByText('Return to an unfinished session action before starting another.'),
    ).toBeVisible()
    await openSessionFromCatalog(page, sessionId)
    const goal = page.getByRole('textbox', { name: 'Goal', exact: true })
    await expect(goal).toHaveValue(goalStatement)
    await expect(goal).toBeEnabled()
    await expect(page.getByRole('button', { name: 'Confirm', exact: true })).toBeEnabled()
    const editedGoal = 'Keep this edited draft'
    await goal.fill(editedGoal)
    const fifth = sessions[4]
    if (!fifth) throw new Error('Missing fifth session fixture')
    await openSessionFromCatalog(page, fifth)
    await expect(page.getByRole('button', { name: 'Set goal', exact: true })).toBeDisabled()
    await openSessionFromCatalog(page, sessionId)
    await expect(goal).toHaveValue(editedGoal)
    if (resolution === 'Escape') await goal.press('Escape')
    else await page.getByRole('button', { name: 'Keep unchanged', exact: true }).click()
    await expect(page.getByRole('region', { name: 'Session workspace', exact: true })).toBeFocused()
    await openSessionFromCatalog(page, fifth)
    await expect(page.getByRole('button', { name: 'Set goal', exact: true })).toBeEnabled()
    await openSessionFromCatalog(page, sessionId)
    await expect(goal).toHaveCount(0)
  })
}

for (const action of [
  { button: 'Approve', path: `approvals/${requestId}`, field: null },
  { button: 'Deny', path: `approvals/${requestId}`, field: 'Note (optional)' },
  { button: 'Cancel turn', path: 'cancel', field: 'Message to continue with' },
  { button: 'Set goal', path: 'goal', field: 'Goal' },
  { button: 'Clear goal', path: 'goal', field: null },
]) {
  test(`successful ${action.button} confirmation restores visible keyboard focus`, async ({
    page,
  }) => {
    const api = await sessionApi(page, true)
    if (action.button === 'Approve' || action.button === 'Deny') {
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    }
    await page.route(`**/api/sessions/${sessionId}/${action.path}`, (route) =>
      route.fulfill({ status: 204 }),
    )
    await openSession(page)
    const opener = page.getByRole('button', { name: action.button, exact: true })
    await opener.press('Enter')
    if (action.field)
      await page.getByRole('textbox', { name: action.field, exact: true }).fill('Requested action')
    await page.getByRole('button', { name: 'Confirm', exact: true }).press('Enter')
    await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
    const workspace = page.getByRole('region', { name: 'Session workspace', exact: true })
    await expect
      .poll(
        async () =>
          (await opener.evaluate((element) => element === document.activeElement)) ||
          (await workspace.evaluate((element) => element === document.activeElement)),
      )
      .toBe(true)
  })
}

for (const transition of ['changes', 'ends']) {
  test(`a fresh cancellation stops submitting when its turn ${transition}`, async ({ page }) => {
    const api = await sessionApi(page, true)
    const requests: string[] = []
    page.on('request', (request) => {
      if (['POST', 'PUT', 'DELETE'].includes(request.method())) requests.push(request.url())
    })
    await openSession(page)
    await page.getByRole('button', { name: 'Cancel turn', exact: true }).click()
    const field = page.getByRole('textbox', { name: 'Message to continue with' })
    await field.fill(successorMessage)
    const confirm = page.getByRole('button', { name: 'Confirm', exact: true })
    await expect(confirm).toBeEnabled()
    api.state.activeTurnId = nextTurnId
    api.state.active = transition === 'changes'
    api.advanceObservation()
    await expect(confirm).toBeDisabled()
    await confirm.evaluate((button) => {
      if (!(button instanceof HTMLButtonElement)) throw new Error('Confirm must be a button')
      button.form?.requestSubmit()
    })
    expect(requests).toEqual([])
    await expect(field).toHaveValue(successorMessage)
  })
}

test('a retained cancellation replays its original turn when another turn awaits approval', async ({
  page,
}) => {
  const api = await sessionApi(page, true)
  const requests: unknown[] = []
  await page.route(`**/api/sessions/${sessionId}/cancel`, (route) => {
    requests.push(route.request().postDataJSON())
    return requests.length === 1 ? route.abort('failed') : route.fulfill({ status: 204 })
  })
  await openSession(page)
  await page.getByRole('button', { name: 'Cancel turn', exact: true }).click()
  await page.getByRole('textbox', { name: 'Message to continue with' }).fill(successorMessage)
  await page.getByRole('button', { name: 'Confirm', exact: true }).click()
  await expect(page.getByRole('alert')).toContainText('Outcome unconfirmed')
  api.state.activeTurnId = nextTurnId
  api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
  api.advanceObservation()
  await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeDisabled()
  await page.getByRole('button', { name: 'Retry same action' }).click()
  await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
  expect(requests).toEqual([
    { command_id: expect.any(String), expected_active_turn_id: turnId, message: successorMessage },
    requests[0],
  ])
})

for (const action of ['Cancel turn', 'Clear goal']) {
  test(`a retained ${action} remains replayable during approval waiting`, async ({ page }) => {
    const api = await sessionApi(page, true)
    const requests: unknown[] = []
    const path = action === 'Cancel turn' ? 'cancel' : 'goal'
    await page.route(`**/api/sessions/${sessionId}/${path}`, (route) => {
      requests.push(route.request().postDataJSON())
      return requests.length === 1 ? route.abort('failed') : route.fulfill({ status: 204 })
    })
    await openSession(page)
    await page.getByRole('button', { name: action, exact: true }).click()
    if (action === 'Cancel turn') {
      await page.getByRole('textbox', { name: 'Message to continue with' }).fill(successorMessage)
    }
    await page.getByRole('button', { name: 'Confirm', exact: true }).click()
    await expect(page.getByRole('alert')).toContainText('Outcome unconfirmed')
    api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    api.advanceObservation()
    await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeDisabled()
    await page.getByRole('button', { name: 'Retry same action' }).click()
    await expect(page.getByText('Action accepted', { exact: true })).toBeVisible()
    expect(requests).toEqual([requests[0], requests[0]])
    await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeEnabled()
  })

  test(`an open ${action} confirmation stops submitting when approval waiting begins`, async ({
    page,
  }) => {
    const api = await sessionApi(page, true)
    const requests: string[] = []
    page.on('request', (request) => {
      if (['POST', 'PUT', 'DELETE'].includes(request.method())) requests.push(request.url())
    })
    await openSession(page)
    await page.getByRole('button', { name: action, exact: true }).click()
    if (action === 'Cancel turn') {
      await page.getByRole('textbox', { name: 'Message to continue with' }).fill(successorMessage)
    }
    const confirm = page.getByRole('button', { name: 'Confirm', exact: true })
    await expect(confirm).toBeEnabled()
    api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    api.advanceObservation()
    await expect(page.getByRole('button', { name: 'Approve', exact: true })).toBeVisible()
    await expect(confirm).toBeDisabled()
    await confirm.evaluate((button) => {
      if (!(button instanceof HTMLButtonElement)) throw new Error('Confirm must be a button')
      button.form?.requestSubmit()
    })
    expect(requests).toEqual([])
    if (action === 'Cancel turn') {
      await expect(page.getByRole('textbox', { name: 'Message to continue with' })).toHaveValue(
        successorMessage,
      )
    }
  })
}

for (const action of ['Approve', 'Clear goal']) {
  test(`Escape from the ${action} opener dismisses its confirmation`, async ({ page }) => {
    const api = await sessionApi(page, true)
    if (action === 'Approve') {
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    }
    const requests: string[] = []
    page.on('request', (request) => {
      if (['POST', 'PUT', 'DELETE'].includes(request.method())) requests.push(request.url())
    })
    await openSession(page)
    const opener = page.getByRole('button', { name: action, exact: true })
    await opener.press('Enter')
    const confirm = page.getByRole('button', { name: 'Confirm', exact: true })
    await expect(confirm).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(confirm).toHaveCount(0)
    await expect(opener).toBeFocused()
    expect(requests).toEqual([])
  })
}

for (const action of ['Approve', 'Deny']) {
  for (const transition of ['changes', 'ends']) {
    test(`a fresh ${action} confirmation stops submitting when its request ${transition}`, async ({
      page,
    }) => {
      const api = await sessionApi(page, true)
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
      const requests: string[] = []
      page.on('request', (request) => {
        if (['POST', 'PUT', 'DELETE'].includes(request.method())) requests.push(request.url())
      })
      await openSession(page)
      await page.getByRole('button', { name: action, exact: true }).click()
      if (action === 'Deny')
        await page.getByRole('textbox', { name: 'Note (optional)' }).fill('Outside scope')
      const confirm = page.getByRole('button', { name: 'Confirm', exact: true })
      await expect(confirm).toBeEnabled()
      api.state.activeState =
        transition === 'changes'
          ? {
              kind: 'awaiting_tool_approval',
              tool_request_id: '20000000-0000-4000-8000-000000000002',
            }
          : { kind: 'running', model_call_id: null }
      api.advanceObservation()
      await expect(confirm).toBeDisabled()
      await confirm.evaluate((button) => {
        if (!(button instanceof HTMLButtonElement)) throw new Error('Confirm must be a button')
        button.form?.requestSubmit()
      })
      expect(requests).toEqual([])
    })
  }
}

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
    await expect(header.getByRole('button', { name: 'Clear goal', exact: true })).toHaveCount(0)
    for (const name of ['Approve', 'Deny', 'Set goal']) {
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

for (const action of [
  { button: 'Set goal', field: 'Goal' },
  { button: 'Deny', field: 'Note (optional)' },
  { button: 'Cancel turn', field: 'Message to continue with' },
]) {
  test(`Escape closes ${action.button} editing and restores its opener`, async ({ page }) => {
    const api = await sessionApi(page, true)
    if (action.button === 'Deny') {
      api.state.activeState = { kind: 'awaiting_tool_approval', tool_request_id: requestId }
    }
    const submissions: string[] = []
    page.on('request', (request) => {
      if (['POST', 'PUT', 'DELETE'].includes(request.method())) submissions.push(request.url())
    })
    await openSession(page)
    const opener = page.getByRole('button', { name: action.button, exact: true })
    await opener.click()
    const field = page.getByRole('textbox', { name: action.field, exact: true })
    await field.fill('Unsubmitted text')
    await field.press('Escape')
    await expect(field).toHaveCount(0)
    await expect(opener).toBeFocused()
    await expect(page).toHaveURL(new RegExp(`session=${sessionId}`))
    expect(submissions).toEqual([])
    await opener.press('Enter')
    await expect(field).toHaveValue('')
  })
}

import { expect, type Page, type TestInfo, test } from '@playwright/test'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 65_536 },
} as const

const approvalSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c61'
const blockedSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c62'
const lostRunnerSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c63'
const idleSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c90'
const approvalTurnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c71'
const blockedTurnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c72'

const attentionFixture = {
  continuation_after_session_id: null,
  cursor: '42',
  summaries: [
    {
      action: 'decide_approval',
      current_turn_id: approvalTurnId,
      goal_block: null,
      judge: { actionable: '2', completed: '7', escalated: '1', failed: '0' },
      last_activity: { kind: 'approval_judge', unix_milliseconds: '1787342400000' },
      session_id: approvalSessionId,
      state: 'awaiting_approval',
    },
    {
      action: 'provide_goal_need',
      current_turn_id: blockedTurnId,
      goal_block: {
        generation: '3',
        need_summary: 'Choose the repository that should receive the release branch.',
        reason: 'user_input_required',
      },
      judge: { actionable: '0', completed: '4', escalated: '0', failed: '0' },
      last_activity: { kind: 'goal', unix_milliseconds: '1787341800000' },
      session_id: blockedSessionId,
      state: 'blocked',
    },
    {
      action: null,
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '12', escalated: '1', failed: '1' },
      last_activity: { kind: 'runner', unix_milliseconds: '1787341200000' },
      session_id: lostRunnerSessionId,
      state: 'runner_lost',
    },
  ],
} as const

const nextAttentionFixture = {
  continuation_after_session_id: null,
  cursor: '42',
  summaries: [
    {
      action: null,
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '2', escalated: '0', failed: '0' },
      last_activity: { kind: 'session', unix_milliseconds: '1787340600000' },
      session_id: idleSessionId,
      state: 'idle',
    },
  ],
} as const

const continuedAttentionFixture = {
  ...attentionFixture,
  continuation_after_session_id: `018f1840-6f3d-7a8b-9c1d-${(
    BigInt(`0x${approvalSessionId.slice(-12)}`) + 31n
  )
    .toString(16)
    .padStart(12, '0')}`,
  summaries: Array.from({ length: 32 }, (_, index) => ({
    ...attentionFixture.summaries[0],
    session_id: `018f1840-6f3d-7a8b-9c1d-${(
      BigInt(`0x${approvalSessionId.slice(-12)}`) + BigInt(index)
    )
      .toString(16)
      .padStart(12, '0')}`,
  })),
} as const

const installAttentionScenario = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention**', (route) => {
    const requestUrl = new URL(route.request().url())
    if (requestUrl.pathname.endsWith('/follow')) {
      return route.fulfill({
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
        contentType: 'application/x-ndjson',
      })
    }
    if (requestUrl.searchParams.has('after_session_id')) {
      return route.fulfill({ json: nextAttentionFixture })
    }
    return route.fulfill({ json: attentionFixture })
  })
}

const installAttentionPagingScenario = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention**', (route) => {
    const requestUrl = new URL(route.request().url())
    if (requestUrl.pathname.endsWith('/follow')) {
      return route.fulfill({
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: continuedAttentionFixture })}\n`,
        contentType: 'application/x-ndjson',
      })
    }
    if (requestUrl.searchParams.has('after_session_id')) {
      return route.fulfill({ json: nextAttentionFixture })
    }
    return route.fulfill({ json: continuedAttentionFixture })
  })
}

const installAttentionReplacementScenario = async (page: Page) => {
  let snapshotRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', (route) => {
    snapshotRequests += 1
    return route.fulfill({
      json: snapshotRequests === 1 ? attentionFixture : nextAttentionFixture,
    })
  })
}

const installFailedAttentionPageScenario = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention**', (route) => {
    const requestUrl = new URL(route.request().url())
    if (requestUrl.pathname.endsWith('/follow')) {
      return route.fulfill({
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: continuedAttentionFixture })}\n`,
        contentType: 'application/x-ndjson',
      })
    }
    if (requestUrl.searchParams.has('after_session_id')) {
      return route.fulfill({
        json: {
          error: {
            code: 'attention_projection_unavailable',
            kind: 'application',
            message: 'the requested page is unavailable',
          },
        },
        status: 503,
      })
    }
    return route.fulfill({ json: continuedAttentionFixture })
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

const installRecoveringMonitorScenario = async (page: Page) => {
  let followRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) => {
    followRequests += 1
    return followRequests === 1
      ? route.fulfill({ body: '{"kind":', contentType: 'application/x-ndjson' })
      : route.fulfill({
          body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
          contentType: 'application/x-ndjson',
        })
  })
  await page.route('**/api/attention', (route) => route.fulfill({ json: attentionFixture }))
  return () => followRequests
}

const installStaleMonitorScenario = async (page: Page) => {
  let followRequests = 0
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) => {
    followRequests += 1
    return route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    })
  })
  await page.route('**/api/attention', (route) => route.fulfill({ json: attentionFixture }))
  return () => followRequests
}

const installNewerHttpSnapshotScenario = async (page: Page) => {
  const newerSnapshot = { ...nextAttentionFixture, cursor: '43' }
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 50))
    await route.fulfill({ json: newerSnapshot })
  })
}

const installDivergentEqualCursorHttpScenario = async (page: Page) => {
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    }),
  )
  await page.route('**/api/attention', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 50))
    await route.fulfill({ json: nextAttentionFixture })
  })
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test('opens and closes the attention inspector without a mouse and restores focus', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.goto('/attention')

  const approval = page.getByRole('button', {
    name: new RegExp(`awaiting approval.*${approvalSessionId}`),
  })
  await approval.focus()
  await approval.press('Enter')
  const close = page.getByRole('button', { name: 'Close attention inspector' })
  await expect(close).toBeFocused()
  await expect(page.getByRole('heading', { name: 'awaiting approval', level: 2 })).toBeVisible()
  await close.press('Escape')
  await expect(page.getByRole('button', { name: 'Close attention inspector' })).toBeHidden()
  await expect(approval).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('replaces the current bounded page instead of accumulating attention history', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await installAttentionPagingScenario(page)
  await page.goto('/attention')

  await expect(page.getByRole('listitem')).toHaveCount(continuedAttentionFixture.summaries.length)
  await page.getByRole('button', { name: /Next page/ }).click()
  await expect(page.getByRole('listitem')).toHaveCount(nextAttentionFixture.summaries.length)
  await expect(page.getByText(idleSessionId)).toBeVisible()
  await expect(page.getByText(approvalSessionId)).toBeHidden()
  await expect(page.getByRole('button', { name: 'Return to live page' })).toBeVisible()
  await expect(
    page.getByRole('heading', {
      name: `${nextAttentionFixture.summaries.length} sessions`,
      level: 2,
    }),
  ).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns to the live page after a paged read fails', async ({ page }) => {
  await installFailedAttentionPageScenario(page)
  await page.goto('/attention')

  await page.getByRole('button', { name: /Next page/ }).click()
  await expect(page.getByRole('heading', { name: 'Attention could not be read' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toBeFocused()
  await page.getByRole('button', { name: 'Return to live page' }).click()

  await expect(page.getByText(approvalSessionId)).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Attention could not be read' })).toBeHidden()
})

test('closes the inspector with global Escape after focus leaves it', async ({ page }) => {
  await installAttentionScenario(page)
  await page.goto('/attention')

  const approval = page.getByRole('button', {
    name: new RegExp(`awaiting approval.*${approvalSessionId}`),
  })
  await approval.click()
  await page.getByRole('button', { name: 'Refresh snapshot' }).focus()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('button', { name: 'Close attention inspector' })).toBeHidden()
  await expect(approval).toBeFocused()
})

test('moves focus to the page heading when refreshed data removes the selection', async ({
  page,
}) => {
  await installAttentionReplacementScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')

  await page
    .getByRole('button', { name: new RegExp(`awaiting approval.*${approvalSessionId}`) })
    .click()
  await page.getByRole('button', { name: 'Refresh snapshot' }).click()

  await expect(page.getByRole('button', { name: 'Close attention inspector' })).toBeHidden()
  await expect(
    page.getByRole('heading', {
      name: `${nextAttentionFixture.summaries.length} sessions`,
      level: 2,
    }),
  ).toBeFocused()
})

test('restarts a failed Attention monitor in place', async ({ page }) => {
  const followRequests = await installRecoveringMonitorScenario(page)
  await page.goto('/attention')

  await expect(page.getByText('Monitor unavailable')).toBeVisible()
  await page.getByRole('button', { name: 'Restart monitor' }).click()

  await expect.poll(followRequests).toBe(2)
  await expect(page.getByText('Monitor paused')).toBeVisible()
})

test('restarts a stale Attention monitor in place', async ({ page }) => {
  const followRequests = await installStaleMonitorScenario(page)
  await page.goto('/attention')

  await expect(page.getByText('Monitor paused')).toBeVisible()
  await page.getByRole('button', { name: 'Refresh snapshot' }).click()

  await expect.poll(followRequests).toBe(2)
})

test('keeps a newer HTTP snapshot than an in-flight follower snapshot', async ({ page }) => {
  await installNewerHttpSnapshotScenario(page)
  await page.goto('/attention')

  await expect(page.getByText(idleSessionId)).toBeVisible()
  await expect(page.getByText(approvalSessionId)).toBeHidden()
  await expect(page.getByText('cursor 43')).toBeVisible()
})

test('rejects a divergent equal-cursor HTTP snapshot after the follower starts', async ({
  page,
}) => {
  await installDivergentEqualCursorHttpScenario(page)
  await page.goto('/attention')

  await expect(page.getByRole('heading', { name: 'Attention could not be read' })).toBeVisible()
  await expect(
    page.getByText('The response did not match the generated web contract.'),
  ).toBeVisible()
})

test('captures the dark attention fleet', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.goto('/attention')
  await expect(page.getByRole('heading', { name: '3 sessions', level: 2 })).toBeVisible()
  await expect(page).toHaveScreenshot('attention-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the light attention workbench inspector', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.goto('/attention')
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await page.getByRole('button', { name: new RegExp(`blocked.*${blockedSessionId}`) }).click()
  await expect(page.getByRole('heading', { name: 'blocked', level: 2 })).toBeVisible()
  await expect(page).toHaveScreenshot('attention-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the focused phone inspector', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/attention')
  await page
    .getByRole('button', { name: new RegExp(`runner lost.*${lostRunnerSessionId}`) })
    .click()
  await expect(page.getByRole('heading', { name: 'runner lost', level: 2 })).toBeVisible()
  await expect(page.getByRole('heading', { name: '3 sessions', level: 2 })).toBeHidden()
  await expect(page).toHaveScreenshot('attention-mobile-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies the density preference to Attention rows', async ({ page }) => {
  await installAttentionScenario(page)
  await page.goto('/attention')

  const row = page.getByRole('listitem').first().getByRole('button')
  await expect(row).toHaveCSS('min-height', '62px')
  await page.getByRole('button', { name: 'Use comfortable density' }).click()
  await expect(row).toHaveCSS('min-height', '78px')
})

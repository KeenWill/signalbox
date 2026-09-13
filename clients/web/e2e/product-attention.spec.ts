import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, type Page, type Route, type TestInfo, test } from './fontTest'

const approvalSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c61'
const blockedSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c62'
const lostRunnerSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c63'
const laterSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c90'
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
      lifecycle_state: 'waiting',
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
      lifecycle_state: 'blocked',
      session_id: blockedSessionId,
      state: 'blocked',
    },
    {
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '12', escalated: '1', failed: '1' },
      last_activity: { kind: 'runner', unix_milliseconds: '1787341200000' },
      lifecycle_state: 'recovering',
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
      action: 'decide_approval',
      current_turn_id: approvalTurnId,
      goal_block: null,
      judge: { actionable: '0', completed: '2', escalated: '0', failed: '0' },
      last_activity: { kind: 'session', unix_milliseconds: '1787340600000' },
      lifecycle_state: 'waiting',
      session_id: laterSessionId,
      state: 'awaiting_approval',
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

const installAdvancingAttentionPageScenario = async (page: Page) => {
  let pagedRequests = 0
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
      pagedRequests += 1
      return route.fulfill({
        json: { ...nextAttentionFixture, cursor: pagedRequests === 1 ? '50' : '45' },
      })
    }
    return route.fulfill({ json: continuedAttentionFixture })
  })
  return () => pagedRequests
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

const installRegressingAttentionPageScenario = async (page: Page) => {
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
      return route.fulfill({ json: { ...nextAttentionFixture, cursor: '41' } })
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

// The monitor goes stale after its single follow response, and every read after the one that
// mounts the page is held open, modelling a daemon whose stream is healthy while its ordinary
// read stalls.
const installHeldSnapshotReadMonitorScenario = async (page: Page) => {
  let followRequests = 0
  let snapshotRequests = 0
  const heldReads: Route[] = []
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention/follow', (route) => {
    followRequests += 1
    return route.fulfill({
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
      contentType: 'application/x-ndjson',
    })
  })
  await page.route('**/api/attention', (route) => {
    snapshotRequests += 1
    if (snapshotRequests > 1) {
      heldReads.push(route)
      return undefined
    }
    return route.fulfill({ json: attentionFixture })
  })
  return {
    followRequests: () => followRequests,
    releaseHeldReads: () =>
      Promise.all(heldReads.map((route) => route.fulfill({ json: attentionFixture }))),
  }
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

const installRegressingAttentionRefetchScenario = async (page: Page) => {
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
      json: snapshotRequests === 1 ? attentionFixture : { ...nextAttentionFixture, cursor: '41' },
    })
  })
  return () => snapshotRequests
}

const skipUnlessLinuxChromium = (testInfo: TestInfo) => {
  test.skip(
    testInfo.project.name !== 'chromium' || process.platform !== 'linux',
    'Chromium on Linux owns pixel evidence',
  )
}

test.beforeEach(async ({ page }) => {
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        cursor: '42',
        total: '0',
        summaries: [],
        continuation: null,
        sort: 'last_activity_descending',
      },
    }),
  )
  await page.route('**/api/sessions/rates**', (route) => route.fulfill({ json: { sessions: [] } }))
})

test('replaces the current bounded page instead of accumulating attention history', async ({
  page,
}) => {
  const problems = watchBrowser(page)
  await installAttentionPagingScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByRole('listitem')).toHaveCount(continuedAttentionFixture.summaries.length)
  await page.getByRole('button', { name: /Next/ }).click()
  await expect(page.getByRole('listitem')).toHaveCount(nextAttentionFixture.summaries.length)
  await expect(page.getByText(laterSessionId)).toBeVisible()
  await expect(page.getByText(approvalSessionId)).toBeHidden()
  await expect(page.getByRole('button', { name: 'First page' })).toBeVisible()
  await expect(
    page.getByRole('heading', {
      name: '1 session needs attention on this page',
      level: 2,
    }),
  ).toBeFocused()
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('returns to the live page after a paged read fails', async ({ page }) => {
  await installFailedAttentionPageScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await page.getByRole('button', { name: /Next/ }).click()
  await expect(page.getByRole('heading', { name: 'Attention failed to load' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toBeFocused()
  await page.getByRole('button', { name: 'First page' }).click()

  await expect(page.getByText(approvalSessionId)).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Attention failed to load' })).toBeHidden()
})

test('rejects a paged Attention response with a regressing cursor', async ({ page }) => {
  await installRegressingAttentionPageScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await page.getByRole('button', { name: /Next/ }).click()

  await expect(page.getByRole('heading', { name: 'Attention failed to load' })).toBeVisible()
  await expect(page.getByText('Unexpected daemon response.')).toBeVisible()
})

test('advances the paged cursor floor after a successful read', async ({ page }) => {
  const pagedRequests = await installAdvancingAttentionPageScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await page.getByRole('button', { name: /Next/ }).click()
  await expect(page.getByText(laterSessionId)).toBeVisible()
  await page.getByRole('button', { name: 'Refresh' }).click()

  await expect.poll(pagedRequests).toBe(2)
  await expect(page.getByText(laterSessionId)).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Attention failed to load' })).toBeVisible()
})

test('restarts a failed Attention monitor in place', async ({ page }) => {
  const followRequests = await installRecoveringMonitorScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByText('Disconnected')).toBeVisible()
  await page.getByRole('button', { name: 'Reconnect' }).click()

  await expect.poll(followRequests).toBe(2)
  await expect(page.getByText('Paused')).toBeVisible()
})

test('restarts a stale Attention monitor in place', async ({ page }) => {
  const followRequests = await installStaleMonitorScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByText('Paused')).toBeVisible()
  await page.getByRole('button', { name: 'Reconnect' }).click()

  await expect.poll(followRequests).toBe(2)
})

test('restarts the Attention monitor while the snapshot read is pending', async ({ page }) => {
  const scenario = await installHeldSnapshotReadMonitorScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByText('Paused')).toBeVisible()
  await page.getByRole('button', { name: 'Reconnect' }).click()

  await expect.poll(scenario.followRequests).toBe(2)
  await scenario.releaseHeldReads()
})

test('keeps a newer HTTP snapshot than an in-flight follower snapshot', async ({ page }) => {
  await installNewerHttpSnapshotScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByText(laterSessionId)).toBeVisible()
  await expect(page.getByText(approvalSessionId)).toBeHidden()
})

test('rejects a divergent equal-cursor HTTP snapshot after the follower starts', async ({
  page,
}) => {
  await installDivergentEqualCursorHttpScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  await expect(page.getByRole('heading', { name: 'Attention failed to load' })).toBeVisible()
  await expect(page.getByText('Unexpected daemon response.')).toBeVisible()
})

test('keeps the live projection when a refresh snapshot regresses', async ({ page }) => {
  const snapshotRequests = await installRegressingAttentionRefetchScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  await expect(page.getByText(approvalSessionId)).toBeVisible()

  await page.getByRole('button', { name: 'Reconnect' }).click()

  await expect.poll(snapshotRequests).toBe(2)
  await expect(page.getByText(approvalSessionId)).toBeVisible()
  await expect(page.getByText(approvalSessionId)).toBeVisible()
  await expect(page.getByText(laterSessionId)).toBeHidden()
})

test('captures the dark attention fleet', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  await expect(
    page.getByRole('heading', { name: '2 sessions need attention on this page', level: 2 }),
  ).toBeVisible()
  await expect.soft(page).toHaveScreenshot('attention-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the light attention filter', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('attention-light.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('captures the phone attention filter', async ({ page }, testInfo) => {
  skipUnlessLinuxChromium(testInfo)
  const problems = watchBrowser(page)
  await installAttentionScenario(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  await expect.soft(page).toHaveScreenshot('attention-mobile-dark.png', { animations: 'disabled' })
  expect(problems).toEqual({ consoleErrors: [], pageErrors: [] })
})

test('applies the density preference to Attention rows', async ({ page }) => {
  await installAttentionScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()

  const row = page.getByRole('listitem').first().getByRole('link')
  await expect(row).toHaveCSS('min-height', '62px')
  await page.getByRole('main').focus()
  await page.keyboard.press('Shift+D')
  await expect(row).toHaveCSS('min-height', '78px')
})

test('uses the available Attention width and keeps arrows inside their rows', async ({ page }) => {
  await installAttentionScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  for (const width of [1440, 1024, 390]) {
    await page.setViewportSize({ width, height: 900 })
    const row = page.getByRole('listitem').first().getByRole('link')
    await expect(row).toBeVisible()
    const workbench = await page.locator('.attention-workbench').boundingBox()
    const list = await page.locator('.attention-list').boundingBox()
    expect(list?.width).toBeGreaterThanOrEqual((workbench?.width ?? 0) - 1)
    const checkArrow = async () => {
      const button = await row.boundingBox()
      const arrow = await row.locator('svg').boundingBox()
      expect(button).not.toBeNull()
      expect(arrow).not.toBeNull()
      expect((arrow?.x ?? 0) + (arrow?.width ?? 0)).toBeLessThan(
        (button?.x ?? 0) + (button?.width ?? 0) - 2,
      )
    }
    await checkArrow()
    await expect(row).toBeVisible()
  }
})

test('opens the session from its attention row', async ({ page }) => {
  await installAttentionScenario(page)
  await page.goto('/sessions')
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  const row = page.getByRole('link', {
    name: new RegExp(`Approval required.*${approvalSessionId}`),
  })
  await expect(row).toBeVisible()
  await row.focus()
  await row.press('Enter')
  await expect(page).toHaveURL(new RegExp(`/sessions\\?.*session=${approvalSessionId}`))
})

for (const { empty, focusElsewhere } of [
  { empty: false, focusElsewhere: false },
  { empty: true, focusElsewhere: false },
  { empty: false, focusElsewhere: true },
]) {
  test(`live attention removal ${focusElsewhere ? 'preserves focus outside the list' : empty ? 'focuses the heading' : 'focuses a surviving row'}`, async ({
    page,
  }) => {
    await installAttentionScenario(page)
    let release = () => {}
    const released = new Promise<void>((resolve) => {
      release = resolve
    })
    await page.route('**/api/attention/follow', async (route) => {
      await released
      const resolved = attentionFixture.summaries.slice(0, empty ? 2 : 1).map((summary) => ({
        ...summary,
        action: null,
        state: 'idle',
        lifecycle_state: 'created',
        current_turn_id: null,
        goal_block: null,
      }))
      await route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n${JSON.stringify({ kind: 'update', cursor: '43', summaries: resolved })}\n`,
      })
    })
    await page.goto('/sessions')
    await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
    const row = page.getByRole('link').filter({ hasText: approvalSessionId })
    await row.focus()
    await expect(row).toBeFocused()
    const filter = page.getByRole('checkbox', { name: 'Needs attention', exact: true })
    if (focusElsewhere) await filter.focus()
    release()
    await expect(row).toHaveCount(0)
    if (focusElsewhere) {
      await expect(filter).toBeFocused()
    } else if (empty) {
      await expect(
        page.getByRole('heading', { name: '0 sessions need attention on this page' }),
      ).toBeFocused()
    } else {
      const remaining = page.getByRole('link').filter({ hasText: blockedSessionId })
      await expect(remaining).toBeFocused()
      await page.keyboard.press('Enter')
      await expect(page).toHaveURL(new RegExp(`session=${blockedSessionId}`))
    }
  })
}

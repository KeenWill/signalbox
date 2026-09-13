import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'
import { sessionApi } from './session-fixture'

const waitingSession = '10000000-0000-4000-8000-000000000001'
const idleSession = '10000000-0000-4000-8000-000000000002'

async function installSessions(page: Pick<Page, 'route'>, laterPage = false) {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route(/\/api\/sessions\?/, (route) =>
    route.fulfill({
      json: {
        cursor: '10',
        total: '0',
        summaries: [],
        continuation: null,
        sort: 'last_activity_descending',
      },
    }),
  )
  await page.route('**/api/sessions/rates**', (route) => route.fulfill({ json: { sessions: [] } }))
  const snapshot = {
    cursor: '10',
    continuation_after_session_id: null,
    summaries: [
      {
        session_id: waitingSession,
        action: 'decide_approval',
        state: 'awaiting_approval',
        lifecycle_state: 'waiting',
        current_turn_id: '30000000-0000-4000-8000-000000000001',
        goal_block: null,
        last_activity: { kind: 'approval_judge', unix_milliseconds: '1787342400000' },
        judge: { actionable: '2', completed: '7', escalated: '1', failed: '0' },
      },
      {
        session_id: idleSession,
        action: null,
        state: 'idle',
        lifecycle_state: 'created',
        current_turn_id: null,
        goal_block: null,
        last_activity: { kind: 'session', unix_milliseconds: '1787342400000' },
        judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
      },
    ],
  }
  const firstPageRows = Array.from({ length: 32 }, (_, index) => ({
    ...snapshot.summaries[1],
    session_id: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`,
  }))
  const firstPage = laterPage
    ? {
        ...snapshot,
        summaries: firstPageRows,
        continuation_after_session_id: firstPageRows.at(-1)?.session_id ?? null,
      }
    : snapshot
  await page.route('**/api/attention{,?*}', (route) =>
    route.fulfill({
      json: new URL(route.request().url()).searchParams.has('after_session_id')
        ? snapshot
        : firstPage,
    }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: firstPage })}\n`,
    }),
  )
}

for (const viewport of [
  { width: 1440, height: 1000 },
  { width: 390, height: 844 },
]) {
  test(`needs attention filters sessions at ${viewport.width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize(viewport)
    await installSessions(page)
    await page.goto('/sessions')
    await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
    await expect(
      page.getByRole('heading', { name: '1 session needs attention on this page' }),
    ).toBeVisible()
    await expect(page.getByText(idleSession, { exact: true })).toHaveCount(0)
    await expect(page.getByText('Needs decision 2', { exact: true })).toBeVisible()
    await expect(page.getByText('Completed 7', { exact: true })).toBeVisible()
    await expect(page.getByRole('link').filter({ hasText: waitingSession })).toBeVisible()
    await page.screenshot({
      path: testInfo.outputPath(`attention-filter-${viewport.width}.png`),
      fullPage: true,
    })
  })
}

test('the attention URL redirects to Sessions without a separate navigation entry', async ({
  page,
}) => {
  await installSessions(page)
  await page.goto('/attention')
  await expect(page).toHaveURL(/\/sessions\?needsAttention=true$/)
  await expect(
    page
      .getByRole('navigation', { name: 'Product', exact: true })
      .getByRole('link', { name: 'Attention', exact: true }),
  ).toHaveCount(0)
  await expect(page.getByRole('checkbox', { name: 'Needs attention', exact: true })).toBeVisible()
})

test('keyboard selection follows visible attention rows', async ({ page }) => {
  await installSessions(page)
  await page.goto('/sessions')
  await expect(page.getByRole('heading', { name: '0 sessions', exact: true })).toBeVisible()
  await page.getByRole('checkbox', { name: 'Needs attention', exact: true }).check()
  const row = page.getByRole('link').filter({ hasText: waitingSession })
  await expect(row).toBeVisible()
  await page.getByRole('main').focus()
  await page.keyboard.press('j')
  await expect(row).toBeFocused()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(new RegExp(`session=${waitingSession}`))
  await page.goBack()
  await expect(page.getByRole('checkbox', { name: 'Needs attention', exact: true })).toBeChecked()
  await expect(row).toBeFocused()
})

for (const laterPage of [false, true]) {
  for (const returnAction of ['Back', 'Escape']) {
    test(`attention filter and launching row survive ${returnAction} from ${laterPage ? 'a later' : 'the first'} page`, async ({
      page,
    }) => {
      await sessionApi(page, true, waitingSession)
      await installSessions(page, laterPage)
      await page.goto('/sessions')
      const filter = page.getByRole('checkbox', { name: 'Needs attention', exact: true })
      await filter.check()
      if (laterPage) await page.getByRole('button', { name: 'Next', exact: true }).click()
      const row = page.getByRole('link').filter({ hasText: waitingSession })
      await row.click()
      await expect(page.locator('.session-compact-header')).toBeVisible()
      if (returnAction === 'Back') await page.goBack()
      else {
        await page.getByRole('main').focus()
        await page.keyboard.press('Escape')
      }
      await expect(page).toHaveURL(/\/sessions\?needsAttention=true$/)
      await expect(filter).toBeChecked()
      await expect(row).toBeFocused()
      await page.route(/\/api\/sessions\?/, (route) =>
        route.fulfill({
          json: {
            cursor: '10',
            total: '1',
            continuation: null,
            sort: 'last_activity_descending',
            summaries: [
              {
                session_id: waitingSession,
                title_summary: 'Waiting session',
                title_truncated: false,
                state: 'idle',
                archived: false,
                action: null,
                active_turn_count: '0',
                queued_turn_count: '0',
                current_turn_id: null,
                goal_block: null,
                repository_watch: null,
                judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
                last_activity: { kind: 'session', unix_microseconds: '1787342400000000' },
              },
            ],
          },
        }),
      )
      if (laterPage)
        await expect(page.getByRole('button', { name: 'First page', exact: true })).toBeVisible()
      await filter.uncheck()
      await expect(page.getByRole('button', { name: /^Waiting session / })).toBeVisible()
      await expect(filter).toBeFocused()
    })
  }
}

test('Go to Attention opens the Sessions filter', async ({ page }) => {
  await installSessions(page)
  await page.goto('/sessions')
  const filter = page.getByRole('checkbox', { name: 'Needs attention', exact: true })
  await expect(filter).not.toBeChecked()
  await page.getByRole('main').focus()
  await page.keyboard.press('g')
  await page.keyboard.press('a')
  await expect(page).toHaveURL(/\/sessions\?needsAttention=true$/)
  await expect(filter).toBeChecked()
  await expect(page.getByRole('link').filter({ hasText: waitingSession })).toBeVisible()
})

for (const activation of ['click', 'ControlOrMeta', 'middle', 'bookmark'] as const) {
  test(`the wordmark opens the same attention route through ${activation}`, async ({
    page,
    context,
  }) => {
    await installSessions(context)
    await page.goto('/sessions')
    const home = page.getByRole('link', { name: 'Signalbox home' })
    await expect(home).toHaveAttribute('href', '/sessions?needsAttention=true')
    let destination = page
    if (activation === 'bookmark') {
      destination = await context.newPage()
      await destination.goto((await home.getAttribute('href')) ?? '')
    } else if (activation === 'click') {
      await home.click()
    } else {
      const popup = context.waitForEvent('page')
      if (activation === 'middle') await home.click({ button: 'middle' })
      else await home.click({ modifiers: [activation] })
      destination = await popup
    }
    await expect(destination).toHaveURL(/\/sessions\?needsAttention=true$/)
    await expect(
      destination.getByRole('checkbox', { name: 'Needs attention', exact: true }),
    ).toBeChecked()
    await expect(destination.getByRole('link').filter({ hasText: waitingSession })).toBeVisible()
    await destination.reload()
    await expect(
      destination.getByRole('checkbox', { name: 'Needs attention', exact: true }),
    ).toBeChecked()
    await destination.getByRole('checkbox', { name: 'Needs attention', exact: true }).uncheck()
    await expect(destination).toHaveURL(/\/sessions$/)
  })
}

for (const activation of ['ControlOrMeta', 'middle', 'copied destination'] as const) {
  test(`attention rows open their session through ${activation}`, async ({ page, context }) => {
    await sessionApi(context, true, waitingSession)
    await installSessions(context)
    await page.goto('/sessions?needsAttention=true')
    const row = page.getByRole('link').filter({ hasText: waitingSession })
    const href = await row.getAttribute('href')
    expect(href).not.toBeNull()
    const search = new URL(href ?? '', page.url()).searchParams
    expect(search.get('session')).toBe(waitingSession)
    expect(search.get('workspace')).toBe('true')
    expect(search.get('needsAttention')).toBe('true')
    let destination: Page
    if (activation === 'copied destination') {
      destination = await context.newPage()
      await destination.goto(href ?? '')
    } else {
      const popup = context.waitForEvent('page')
      if (activation === 'middle') await row.click({ button: 'middle' })
      else await row.click({ modifiers: [activation] })
      destination = await popup
    }
    await expect(destination).toHaveURL(new RegExp(`session=${waitingSession}`))
    await expect(destination.locator('.session-compact-header')).toBeVisible()
    await expect(page).toHaveURL(/\/sessions\?needsAttention=true$/)
    await destination.getByRole('main').focus()
    await destination.keyboard.press('Escape')
    await expect(destination).toHaveURL(/\/sessions\?needsAttention=true$/)
    await expect(destination.getByRole('checkbox', { name: 'Needs attention' })).toBeChecked()
  })
}

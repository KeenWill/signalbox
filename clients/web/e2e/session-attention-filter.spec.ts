import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'

const waitingSession = '10000000-0000-4000-8000-000000000001'
const idleSession = '10000000-0000-4000-8000-000000000002'

async function installSessions(page: Page) {
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/sessions?**', (route) =>
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
  await page.route('**/api/attention', (route) => route.fulfill({ json: snapshot }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot })}\n`,
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
    const link = page.getByRole('link').filter({ hasText: waitingSession })
    await expect(link).toHaveAttribute('href', new RegExp(`session=${waitingSession}`))
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
  await expect(page).toHaveURL(/\/sessions$/)
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
})

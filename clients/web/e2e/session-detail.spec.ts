import { expect, type Page, test } from '@playwright/test'
import { webContractBootstrapFixture } from '../src/product.fixture'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
  resultCursor,
  toolResultItem,
} from './session-detail-fixture'

async function openDetails(page: Page, mismatch = false) {
  const reads: URL[] = []
  await page.addInitScript(
    ({ sessionId }) => {
      const request = window.fetch
      window.fetch = (input, init) =>
        String(input).endsWith(`/sessions/${sessionId}/follow`)
          ? Promise.resolve(
              new Response(
                new ReadableStream({
                  start(controller) {
                    controller.enqueue(
                      new TextEncoder().encode(
                        `${JSON.stringify({
                          kind: 'snapshot',
                          snapshot: {
                            session_id: sessionId,
                            observed_through: '5',
                            active: null,
                            queued_turn_count: '0',
                            queued_turn_ids: [],
                            reconciliation: null,
                            runner: null,
                          },
                        })}\n`,
                      ),
                    )
                  },
                }),
                { headers: { 'content-type': 'application/x-ndjson' } },
              ),
            )
          : request(input, init)
    },
    { sessionId: detailSessionId },
  )
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/attention', (route) =>
    route.fulfill({ json: { cursor: '0', summaries: [], continuation_after_session_id: null } }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: { cursor: '0', summaries: [], continuation_after_session_id: null } })}\n`,
    }),
  )
  await page.route(`**/api/sessions/${detailSessionId}**`, (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: detailLive })
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: detailWindow })
    if (url.pathname.endsWith('/timeline-detail')) {
      reads.push(url)
      if (url.searchParams.get('first') !== url.searchParams.get('through'))
        return route.fulfill({ json: detailPage(detailItems.slice(0, 2), resultCursor) })
      const selected = detailItems.find(
        (item) => item.address.event_sequence === url.searchParams.get('first'),
      )
      if (!selected)
        return route.fulfill({
          status: 404,
          json: { error: { code: 'missing', message: 'Fixture detail missing' } },
        })
      const terminal = detailItems[4]
      if (mismatch && selected.kind === 'tool_batch_transition' && terminal)
        return route.fulfill({
          json: detailPage([{ ...terminal, address: selected.address }]),
        })
      if (url.searchParams.get('cursor_field') === 'tool_result')
        return route.fulfill({ json: detailPage([toolResultItem()]) })
      return route.fulfill({
        json: detailPage(
          [selected],
          selected.kind === 'tool_batch_transition' ? resultCursor : null,
        ),
      })
    }
    return route.fulfill({
      json: {
        session_id: detailSessionId,
        first_address: { event_sequence: '1' },
        latest_address: { event_sequence: '5' },
        observed_through: '5',
        sizes: {
          item_count: '5',
          projected_text_bytes: '256',
          projected_structured_bytes: String(detailWindow.projected_structured_bytes),
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        work: { active_turn_count: '0', queued_turn_count: '0' },
      },
    })
  })
  await page.goto('/sessions?workspace=true')
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(detailSessionId)
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await expect(page.getByRole('heading', { name: detailSessionId })).toBeVisible()
  return reads
}

const toolRow = (page: Page) =>
  page.getByRole('option').filter({ hasText: 'tool batch transition' })

test('opens tool arguments and follows the typed result continuation', async ({ page }) => {
  const reads = await openDetails(page)
  await toolRow(page).click()
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toContainText(
    'release status --json',
  )
  await page.getByRole('button', { name: 'Load next detail chunk', exact: true }).press('Enter')
  await expect(page.getByRole('region', { name: 'Tool result' })).toContainText('"checks":"passed"')
  expect(reads.at(-1)?.searchParams.get('cursor_field')).toBe('tool_result')
  expect(reads.at(-1)?.searchParams.get('cursor_member')).toBe('0')
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toHaveCount(0)
  await expect(toolRow(page)).toContainText('Tool result')
  await page.getByRole('button', { name: 'Return to event' }).click()
  await expect(page.getByRole('listbox', { name: 'Session timeline' })).toBeFocused()
})

test('fails closed when detail belongs to a different event kind', async ({ page }) => {
  await openDetails(page, true)
  await toolRow(page).click()
  await expect(page.getByRole('alert').filter({ hasText: 'Detail rejected' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toHaveCount(0)
})

test('opens approval and provider detail independently with the keyboard', async ({ page }) => {
  await openDetails(page)
  await page.getByRole('option').filter({ hasText: 'tool approval decided' }).press('Enter')
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toContainText(
    'operator decision',
  )
  await page.getByRole('option').filter({ hasText: 'model call transition' }).press('Enter')
  await expect(page.getByRole('region', { name: 'Model response' })).toContainText('checks passed')
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toBeVisible()
})

test('captures sessions detail evidence', async ({ page, browserName }) => {
  test.skip(browserName !== 'chromium', 'Chromium owns pixel evidence')
  await page.setViewportSize({ width: 1440, height: 1200 })
  await openDetails(page)
  await toolRow(page).click()
  await page.getByRole('button', { name: 'Load next detail chunk', exact: true }).click()
  await expect(page.getByRole('region', { name: 'Tool result' })).toBeVisible()
  await expect.soft(page).toHaveScreenshot('sessions-detail-desktop-dark.png')
  await page.getByRole('option').filter({ hasText: 'tool approval decided' }).click()
  await page.getByRole('region', { name: 'Approval rationale' }).scrollIntoViewIfNeeded()
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toBeVisible()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('sessions-detail-desktop-light.png')
  await page.setViewportSize({ width: 390, height: 844 })
  await page.getByRole('region', { name: 'Approval rationale' }).scrollIntoViewIfNeeded()
  await expect.soft(page).toHaveScreenshot('sessions-detail-mobile-light.png')
})

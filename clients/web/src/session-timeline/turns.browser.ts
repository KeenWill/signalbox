import { expect, type Page, test } from '../../e2e/fontTest'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailTurnId,
  detailWindow,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import { transcriptFixture } from './transcript.fixture'

async function turnApi(page: Page) {
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: detailWindow })
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: detailLive })
    if (url.pathname.endsWith('/timeline-detail')) {
      const item = detailItems.find(
        (entry) =>
          entry.address.event_sequence ===
          (url.searchParams.get('cursor_address') ?? url.searchParams.get('first') ?? '1'),
      )
      if (url.searchParams.get('cursor_field') === 'tool_result')
        return route.fulfill({ json: detailPage([toolResultItem()]) })
      const items = item
        ? [
            item.body.type === 'user_input'
              ? { ...item, body: { ...item.body, attachments: [] } }
              : item,
          ]
        : []
      return route.fulfill({
        json: detailPage(items, item?.body.type === 'tool_batch' ? resultCursor : null),
      })
    }
    if (url.pathname === `/api/sessions/${detailSessionId}`)
      return route.fulfill({
        json: {
          session_id: detailSessionId,
          supervision: null,
          repository_watch: null,
          workspace_root_kind: null,
          sizes: {
            item_count: '5',
            projected_text_bytes: '300',
            projected_structured_bytes: String(detailWindow.projected_structured_bytes),
            referenced_blob_count: '0',
            referenced_blob_bytes: '0',
          },
          first_address: { event_sequence: '1' },
          latest_address: { event_sequence: '5' },
          observed_through: '5',
          work: { active_turn_count: '0', queued_turn_count: '0' },
        },
      })
    return route.fulfill({ json: transcriptFixture(url) })
  })
}

test('shows final turn text and a tool chip while keeping lifecycle noise closed', async ({
  page,
}, testInfo) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(
    transcript.getByText('The release checks passed. Publishing remains unapproved.'),
  ).toBeVisible()
  await expect(
    transcript.getByText('Inspect the release status and retain the result.'),
  ).toBeVisible()
  await expect(transcript.getByText('Turn completed', { exact: true })).toHaveCount(0)
  const chip = transcript.getByRole('button', { name: 'exec_command', exact: true })
  await expect(chip).toHaveAttribute('aria-expanded', 'false')
  await chip.click()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'release status',
  )
  await expect(chip).toHaveAttribute('aria-expanded', 'true')
  await page.screenshot({ path: testInfo.outputPath('turn-summary.png') })
})

test('persists levels, reads turn detail, and keeps a linked event visible', async ({
  page,
}, testInfo) => {
  await turnApi(page)
  const turnReads: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/turns/')) turnReads.push(request.url())
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
  await page.reload()
  await expect(page.getByRole('radio', { name: 'Tools', exact: true })).toBeChecked()
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect.poll(() => turnReads.length).toBeGreaterThan(0)
  await expect(
    transcript.getByText('Publishing needs an operator decision during the release window.', {
      exact: false,
    }),
  ).toBeVisible()
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  await expect(
    transcript.getByText('Publishing needs an operator decision during the release window.', {
      exact: false,
    }),
  ).toHaveCount(0)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}&around=3`)
  const linked = transcript.locator('[data-event-sequence="3"]')
  await expect(linked).toBeInViewport()
  await expect(linked).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('linked-turn-detail.png') })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}&turn=${detailTurnId}`)
  await expect
    .poll(() => turnReads.some((url) => !new URL(url).searchParams.has('cursor_address')))
    .toBe(true)
  await expect(
    transcript.getByText('Inspect the release status and retain the result.'),
  ).toBeVisible()
})

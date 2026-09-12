import { expect, test } from '../../e2e/fontTest'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
  resultCursor,
} from '../../e2e/session-detail-fixture'
import { transcriptFixture } from './transcript.fixture'

test('shows final turn text and a tool chip while keeping lifecycle noise closed', async ({
  page,
}, testInfo) => {
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
        (entry) => entry.address.event_sequence === url.searchParams.get('first'),
      )
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
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
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

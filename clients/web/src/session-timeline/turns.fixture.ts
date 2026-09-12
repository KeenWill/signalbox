import type { Page } from '../../e2e/fontTest'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import { transcriptFixture } from './transcript.fixture'

export async function turnApi(page: Page) {
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

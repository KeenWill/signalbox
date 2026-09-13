import type { Page } from '../../e2e/fontTest'
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
import type { WebSessionTimelineDetail } from '../generated/web-contract.mjs'
import { transcriptFixture } from './transcript.fixture'

export async function turnApi(page: Page, turnId = detailTurnId, entries = detailItems) {
  const withTurn = (item: WebSessionTimelineDetail): WebSessionTimelineDetail => ({
    ...item,
    body:
      'turn_id' in item.body && item.body.turn_id === detailTurnId
        ? { ...item.body, turn_id: turnId }
        : item.body,
  })
  const window = {
    ...detailWindow,
    items: entries.map(({ address, kind }) => ({
      address,
      kind,
      projected_structured_bytes: 64 + kind.length,
    })),
    projected_structured_bytes: entries.reduce((sum, item) => sum + 64 + item.kind.length, 0),
  }
  const latest = entries.at(-1)?.address.event_sequence ?? '1'
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: window })
    if (url.pathname.endsWith('/live'))
      return route.fulfill({ json: { ...detailLive, observed_through: latest } })
    if (url.pathname.endsWith('/timeline-detail')) {
      const item = entries
        .map(withTurn)
        .find(
          (entry) =>
            entry.address.event_sequence ===
            (url.searchParams.get('cursor_address') ?? url.searchParams.get('first') ?? '1'),
        )
      if (url.searchParams.get('cursor_field') === 'tool_result')
        return route.fulfill({
          json: detailPage([
            {
              ...withTurn(toolResultItem()),
              address: { event_sequence: url.searchParams.get('cursor_address') ?? '2' },
            },
          ]),
        })
      const items = item
        ? [
            item.body.type === 'user_input'
              ? { ...item, body: { ...item.body, attachments: [] } }
              : item,
          ]
        : []
      return route.fulfill({
        json: detailPage(
          items,
          item?.body.type === 'tool_batch' && item.body.tools.length > 0
            ? { ...resultCursor, body: { ...resultCursor.body, address: item.address } }
            : null,
        ),
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
            item_count: String(entries.length),
            projected_text_bytes: '300',
            projected_structured_bytes: String(window.projected_structured_bytes),
            referenced_blob_count: '0',
            referenced_blob_bytes: '0',
          },
          first_address: { event_sequence: '1' },
          latest_address: { event_sequence: latest },
          observed_through: latest,
          work: { active_turn_count: '0', queued_turn_count: '0' },
        },
      })
    return route.fulfill({ json: transcriptFixture(url) })
  })
}

export async function toolGoalApi(page: Page, type: 'blocked' | 'achieved') {
  await turnApi(page)
  const prefix = type === 'blocked' ? 'Need ' : 'Release '
  const suffix = type === 'blocked' ? 'approval.' : 'verified.'
  const goalCursor = {
    type: 'more_body' as const,
    body: {
      address: { event_sequence: '2' },
      field: 'goal_text' as const,
      member_index: 0,
      offset_bytes: '0',
    },
  }
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('cursor_address') !== '2') return route.fallback()
    if (url.searchParams.get('cursor_field') === 'tool_result')
      return route.fulfill({ json: detailPage([toolResultItem()], goalCursor) })
    if (url.searchParams.get('cursor_field') !== 'goal_text') return route.fallback()
    const first = url.searchParams.get('cursor_offset') === '0'
    const next = first
      ? { ...goalCursor, body: { ...goalCursor.body, offset_bytes: String(prefix.length) } }
      : null
    const text = {
      text: first ? prefix : suffix,
      offset_bytes: first ? '0' : String(prefix.length),
      total_bytes: String(prefix.length + suffix.length),
      continuation: next?.body ?? null,
    }
    const original = toolResultItem()
    if (original.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
    return route.fulfill({
      json: detailPage(
        [
          {
            ...original,
            projected_body_bytes: 128 + text.text.length,
            body: {
              ...original.body,
              projected_member_index: 0,
              tools: [],
              goal_events: [
                type === 'blocked'
                  ? { type, generation: '1', reason: 'user_input_required', text }
                  : { type, generation: '1', text },
              ],
            },
          },
        ],
        next,
      ),
    })
  })
  return { prefix, suffix }
}

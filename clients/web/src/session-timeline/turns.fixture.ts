import type { Page } from '../../e2e/fontTest'
import {
  detailExcerpt,
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
          item?.body.type === 'tool_batch'
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

export function retriedToolItems(): WebSessionTimelineDetail[] {
  const result = toolResultItem()
  const input = detailItems[0]
  const response = detailItems[3]
  const closed = detailItems[4]
  if (
    result.body.type !== 'tool_batch' ||
    input?.body.type !== 'user_input' ||
    !response ||
    !closed
  )
    throw new Error('Retry fixture missing')
  const tool = result.body.tools[0]
  if (tool?.evidence.type !== 'physical_attempt') throw new Error('Physical attempt missing')
  const failure = detailExcerpt('Runner disconnected during the release check.')
  const argumentsText = detailExcerpt('{"cmd":"release status --json"}')
  const nextInput = detailExcerpt('Check the next release too.')
  return [
    input,
    {
      ...result,
      address: { event_sequence: '2' },
      projected_body_bytes: 128 + Number(argumentsText.total_bytes),
      body: {
        ...result.body,
        tools: [
          {
            ...tool,
            arguments: argumentsText,
            evidence: { type: 'request_only' },
          },
        ],
      },
    },
    {
      ...result,
      address: { event_sequence: '3' },
      projected_body_bytes: 128 + Number(argumentsText.total_bytes) + Number(failure.total_bytes),
      body: {
        ...result.body,
        tools: [
          {
            ...tool,
            arguments: argumentsText,
            evidence: {
              ...tool.evidence,
              state: 'known_failed',
              cause: 'crash_lost',
              result: null,
              result_present: false,
              failure,
              failure_present: true,
            },
          },
        ],
      },
    },
    {
      ...input,
      address: { event_sequence: '4' },
      projected_body_bytes: 128 + Number(nextInput.total_bytes),
      body: {
        ...input.body,
        turn_id: '00000000-0000-0000-0000-000000000126',
        text: nextInput,
      },
    },
    {
      ...result,
      address: { event_sequence: '5' },
      projected_body_bytes: result.projected_body_bytes + Number(argumentsText.total_bytes),
      body: {
        ...result.body,
        tools: [
          {
            ...tool,
            arguments: argumentsText,
            evidence: { ...tool.evidence, attempt_id: '00000000-0000-0000-0000-000000000127' },
          },
        ],
      },
    },
    { ...response, address: { event_sequence: '6' } },
    { ...closed, address: { event_sequence: '7' } },
  ]
}

import {
  decodeWebSessionTimelineDetailPage,
  type WebBlobDescriptor,
  type WebTimelineToolMediaReference,
} from '../src/generated/web-contract.mjs'
import { expect, type Page } from './fontTest'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
  resultCursor,
  toolResultItem,
} from './session-detail-fixture'

export async function openAttachmentConversation(
  page: Page,
  descriptor: WebBlobDescriptor,
  toolMedia?: WebTimelineToolMediaReference,
) {
  const items = detailItems
    .filter(
      (item) =>
        (toolMedia || item.body.type !== 'tool_batch') &&
        item.body.type !== 'tool_approval_decision',
    )
    .map((item) => {
      if (toolMedia && item.body.type === 'tool_batch') {
        const result = item
        if (result.body.type !== 'tool_batch') throw new Error('Tool fixture required')
        return {
          ...result,
          body: {
            ...result.body,
            tools: result.body.tools.map((tool) => ({
              ...tool,
              tool_name: 'file_read',
              evidence:
                tool.evidence.type === 'physical_attempt'
                  ? { ...tool.evidence, result_media_reference: toolMedia }
                  : tool.evidence,
            })),
          },
        }
      }
      return item.body.type === 'user_input'
        ? {
            ...item,
            body: {
              ...item.body,
              attachments: toolMedia
                ? []
                : [
                    {
                      blob_id: descriptor.digest,
                      length_bytes: descriptor.byte_length,
                      media_type: descriptor.declared_media_type,
                    },
                  ],
            },
          }
        : item
    })
  decodeWebSessionTimelineDetailPage(detailPage(items))
  const windowItems = detailWindow.items.filter((entry) =>
    items.some((item) => item.address.event_sequence === entry.address.event_sequence),
  )
  const timelineWindow = {
    ...detailWindow,
    items: windowItems,
    projected_structured_bytes: windowItems.reduce(
      (sum, item) => sum + item.projected_structured_bytes,
      0,
    ),
  }
  await page.addInitScript(
    ({ sessionId, snapshot }) => {
      const request = window.fetch
      window.fetch = (input, init) =>
        String(input).endsWith(`/sessions/${sessionId}/follow`)
          ? Promise.resolve(
              new Response(
                new ReadableStream({
                  start(controller) {
                    controller.enqueue(
                      new TextEncoder().encode(
                        `${JSON.stringify({ kind: 'snapshot', snapshot })}\n`,
                      ),
                    )
                  },
                }),
                { headers: { 'content-type': 'application/x-ndjson' } },
              ),
            )
          : request(input, init)
    },
    { sessionId: detailSessionId, snapshot: detailLive },
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
    const turnAddress = url.pathname.includes('/turns/')
      ? url.searchParams.get('cursor_address')
      : null
    if (turnAddress) {
      url.searchParams.set('first', turnAddress)
      url.searchParams.set('through', turnAddress)
    }
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: detailLive })
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: timelineWindow })
    if (toolMedia && url.pathname.endsWith('/timeline-detail')) {
      const first = BigInt(url.searchParams.get('first') ?? '0')
      const through = BigInt(url.searchParams.get('through') ?? '5')
      if (url.searchParams.get('cursor_field') === 'tool_result') {
        const result = toolResultItem()
        if (result.body.type !== 'tool_batch') throw new Error('Tool result required')
        const pageResult = {
          ...result,
          body: {
            ...result.body,
            tools: result.body.tools.map((tool) => ({
              ...tool,
              tool_name: 'file_read',
              evidence:
                tool.evidence.type === 'physical_attempt'
                  ? { ...tool.evidence, result_media_reference: toolMedia }
                  : tool.evidence,
            })),
          },
        }
        return route.fulfill({
          json: detailPage([
            pageResult,
            ...items.filter(
              (item) =>
                BigInt(item.address.event_sequence) > 2n &&
                BigInt(item.address.event_sequence) <= through,
            ),
          ]),
        })
      }
      if (first <= 2n && through >= 2n)
        return route.fulfill({
          json: detailPage(
            items.filter(
              (item) =>
                BigInt(item.address.event_sequence) >= first &&
                BigInt(item.address.event_sequence) <= 2n,
            ),
            resultCursor,
          ),
        })
    }
    if (url.pathname.endsWith('/timeline-detail'))
      return route.fulfill({
        json: detailPage(
          items.filter(
            (item) =>
              BigInt(item.address.event_sequence) >= BigInt(url.searchParams.get('first') ?? '0') &&
              BigInt(item.address.event_sequence) <= BigInt(url.searchParams.get('through') ?? '5'),
          ),
        ),
      })
    return route.fulfill({
      json: {
        session_id: detailSessionId,
        first_address: { event_sequence: '1' },
        latest_address: { event_sequence: '5' },
        observed_through: '5',
        supervision: null,
        repository_watch: null,
        workspace_root_kind: null,
        sizes: {
          item_count: String(items.length),
          projected_text_bytes: '256',
          projected_structured_bytes: String(timelineWindow.projected_structured_bytes),
          referenced_blob_count: '1',
          referenced_blob_bytes: descriptor.byte_length,
        },
        work: { active_turn_count: '0', queued_turn_count: '0' },
      },
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await expect(page.getByRole('list', { name: 'Attachments' })).toBeVisible()
}

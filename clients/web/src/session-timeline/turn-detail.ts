import {
  decodeWebApiErrorResponse,
  decodeWebSessionTimelineDetailPage,
  type WebSessionTimelineDetailPage,
  type WebTimelineDetailContinuation,
} from '../generated/web-contract.mjs'
import { ProductRequestError, type SessionTranscriptLimits } from '../product'
import { validateDetailContinuation } from './model'
import { detailTurnId } from './turns'

export async function readTurnTranscript(
  sessionId: string,
  turnId: string,
  continuation: WebTimelineDetailContinuation | null,
  limits: SessionTranscriptLimits,
  signal?: AbortSignal,
  previous?: WebSessionTimelineDetailPage,
): Promise<WebSessionTimelineDetailPage> {
  if (
    !Number.isSafeInteger(limits.max_timeline_detail_bytes) ||
    limits.max_timeline_detail_bytes > 65536 ||
    limits.max_timeline_detail_bytes < limits.min_timeline_detail_bytes ||
    limits.max_timeline_detail_items < 1
  )
    throw new TypeError('Invalid advertised timeline detail limits')
  const query = new URLSearchParams({
    max_items: '1',
    max_bytes: String(limits.max_timeline_detail_bytes),
  })
  if (continuation?.type === 'more_at')
    query.set('cursor_address', continuation.address.event_sequence)
  if (continuation?.type === 'more_body') {
    query.set('cursor_address', continuation.body.address.event_sequence)
    query.set('cursor_field', continuation.body.field)
    query.set('cursor_member', String(continuation.body.member_index))
    query.set('cursor_offset', continuation.body.offset_bytes)
  }
  const response = await fetch(
    `/api/sessions/${encodeURIComponent(sessionId)}/turns/${encodeURIComponent(turnId)}/timeline-detail?${query}`,
    { credentials: 'same-origin', signal },
  )
  // One detail item can reference 256 blobs; allow their encoded metadata plus escaped text.
  const maximumBytes = limits.max_timeline_detail_bytes * 7 + 256 * 1024
  const reader = response.body?.getReader()
  if (!reader) throw new TypeError('Turn detail response has no body')
  const chunks: Uint8Array[] = []
  let size = 0
  for (;;) {
    const chunk = await reader.read()
    if (chunk.done) break
    size += chunk.value.byteLength
    if (size > maximumBytes) {
      await reader.cancel()
      throw new TypeError('Turn detail response exceeds its byte limit')
    }
    chunks.push(chunk.value)
  }
  const bytes = new Uint8Array(size)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  const payload: unknown = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
  if (!response.ok)
    throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
  const page = decodeWebSessionTimelineDetailPage(payload)
  if (
    page.session_id !== sessionId ||
    page.items.length > 1 ||
    page.projected_body_bytes > limits.max_timeline_detail_bytes ||
    page.items.some((item) => detailTurnId(item) !== null && detailTurnId(item) !== turnId)
  )
    throw new TypeError('Turn detail does not match the requested turn or limits')
  validateDetailContinuation(page, continuation, previous)
  return page
}

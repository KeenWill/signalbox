import { afterEach, expect, it, vi } from 'vitest'
import { followSession, readSessionTranscript, submitSessionInput } from './product'

const sessionId = '00000000-0000-0000-0000-000000000991'
const snapshot = (cursor: string) => ({
  session_id: sessionId,
  observed_through: cursor,
  active: null,
  queued_turn_count: '0',
  queued_turn_ids: [],
  reconciliation: null,
  runner: null,
})
const stream = (...events: unknown[]) =>
  new Response(events.map((event) => JSON.stringify(event)).join('\n') + '\n', {
    headers: { 'content-type': 'application/x-ndjson' },
  })
afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

it('backs off repeated resyncs and cancels the wait without opening another request', async () => {
  vi.useFakeTimers()
  const fetch = vi.fn(async (url: string) =>
    url.endsWith('/live')
      ? Response.json(snapshot('41'))
      : stream(
          { kind: 'snapshot', snapshot: snapshot('41') },
          { kind: 'resync_required', cursor: '42' },
        ),
  )
  vi.stubGlobal('fetch', fetch)
  const controller = new AbortController()
  const follow = followSession(sessionId, controller.signal)
  for (let count = 0; count < 6; count++) await follow.next()
  expect(fetch).toHaveBeenCalledTimes(4)
  const delayed = follow.next()
  await vi.advanceTimersByTimeAsync(999)
  expect(fetch).toHaveBeenCalledTimes(4)
  await vi.advanceTimersByTimeAsync(1)
  expect((await delayed).value).toEqual({ kind: 'snapshot', snapshot: snapshot('41') })
  await follow.next()
  await follow.next()
  expect(fetch).toHaveBeenCalledTimes(6)
  const cancelled = follow.next()
  await vi.advanceTimersByTimeAsync(999)
  expect(fetch).toHaveBeenCalledTimes(6)
  controller.abort()
  expect((await cancelled).done).toBe(true)
  expect(vi.getTimerCount()).toBe(0)
  expect(fetch).toHaveBeenCalledTimes(6)
})

it('resynchronizes a live gap from a fresh snapshot and skips duplicate durable events', async () => {
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(snapshot('41')))
    .mockResolvedValueOnce(
      stream(
        { kind: 'snapshot', snapshot: snapshot('41') },
        { kind: 'resync_required', cursor: '45' },
      ),
    )
    .mockResolvedValueOnce(Response.json(snapshot('45')))
    .mockResolvedValueOnce(
      stream(
        { kind: 'snapshot', snapshot: snapshot('45') },
        {
          kind: 'durable',
          cursor: '45',
          address: { event_sequence: '45' },
          event_kind: 'turn_completed',
        },
        {
          kind: 'durable',
          cursor: '46',
          address: { event_sequence: '46' },
          event_kind: 'input_accepted',
        },
      ),
    )
  vi.stubGlobal('fetch', fetch)
  const follow = followSession(sessionId, new AbortController().signal)
  expect((await follow.next()).value).toEqual({ kind: 'snapshot', snapshot: snapshot('41') })
  expect((await follow.next()).value).toEqual({ kind: 'snapshot', snapshot: snapshot('41') })
  expect((await follow.next()).value).toEqual({ kind: 'resync_required', cursor: '45' })
  expect((await follow.next()).value).toEqual({ kind: 'snapshot', snapshot: snapshot('45') })
  expect((await follow.next()).value).toEqual({ kind: 'snapshot', snapshot: snapshot('45') })
  expect((await follow.next()).value).toMatchObject({ kind: 'durable', cursor: '46' })
  await follow.return(undefined)
  expect(fetch.mock.calls.map(([url]) => url)).toEqual([
    `/api/sessions/${sessionId}/live`,
    `/api/sessions/${sessionId}/follow`,
    `/api/sessions/${sessionId}/live`,
    `/api/sessions/${sessionId}/follow`,
  ])
})

it('cancels an over-budget follow response before accumulating another item', async () => {
  const cancel = vi.fn()
  vi.stubGlobal(
    'fetch',
    vi
      .fn()
      .mockResolvedValueOnce(Response.json(snapshot('41')))
      .mockResolvedValueOnce(
        new Response(
          new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode('x'.repeat(65537)))
            },
            cancel,
          }),
          { headers: { 'content-type': 'application/x-ndjson' } },
        ),
      ),
  )
  const follow = followSession(sessionId, new AbortController().signal)
  await follow.next()
  await expect(follow.next()).rejects.toThrow('Session follow item exceeds its byte limit')
  expect(cancel).toHaveBeenCalledOnce()
})

it('uses the exact command identity and text on every explicit submission attempt', async () => {
  const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }))
  vi.stubGlobal('fetch', fetch)
  const input = {
    command_id: '00000000-0000-0000-0000-000000000992',
    message: 'Continue the recorded conversation.',
  }
  await submitSessionInput(sessionId, input)
  await submitSessionInput(sessionId, input)
  expect(fetch.mock.calls[0]).toEqual(fetch.mock.calls[1])
  expect(fetch.mock.calls[0]?.[1]).toMatchObject({ method: 'POST', body: JSON.stringify(input) })
})

it('uses body continuations to replace one bounded transcript region', async () => {
  const fetch = vi.fn().mockResolvedValue(
    Response.json({
      session_id: sessionId,
      items: [],
      projected_body_bytes: 0,
      continuation: null,
    }),
  )
  vi.stubGlobal('fetch', fetch)
  await readSessionTranscript(sessionId, '41', '46', {
    type: 'more_body',
    body: {
      address: { event_sequence: '44' },
      field: 'model_response',
      member_index: 0,
      offset_bytes: '65000',
    },
  })
  const url = new URL(fetch.mock.calls[0]?.[0], 'http://localhost')
  expect(Object.fromEntries(url.searchParams)).toEqual({
    first: '41',
    through: '46',
    max_items: '8',
    max_bytes: '65536',
    cursor_address: '44',
    cursor_field: 'model_response',
    cursor_member: '0',
    cursor_offset: '65000',
  })
})

it('reads a full attachment-heavy text page within the derived response ceiling', async () => {
  const items = Array.from({ length: 8 }, (_, index) => ({
    address: { event_sequence: String(index + 1) },
    kind: 'input_accepted',
    projected_body_bytes: 128 + 8000,
    body: {
      type: 'user_input',
      turn_id: sessionId,
      text: {
        text: '\u0001'.repeat(8000),
        offset_bytes: '0',
        total_bytes: '8000',
        continuation: null,
      },
      attachments: Array.from({ length: 256 }, () => ({
        blob_id: `sha256:${'f'.repeat(64)}`,
        length_bytes: '18446744073709551615',
        media_type: '"'.repeat(255),
      })),
    },
  }))
  const page = { session_id: sessionId, items, projected_body_bytes: 8 * 8128, continuation: null }
  const response = Response.json(page)
  expect((await response.clone().arrayBuffer()).byteLength).toBeGreaterThan(7 * 65536)
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response))
  expect(await readSessionTranscript(sessionId, '1', '8', null)).toEqual(page)
})

const inputPage = (count: number, textBytes = 1) => {
  const items = Array.from({ length: count }, (_, index) => ({
    address: { event_sequence: String(index + 1) },
    kind: 'input_accepted',
    projected_body_bytes: 128 + textBytes,
    body: {
      type: 'user_input',
      turn_id: sessionId,
      text: {
        text: 'x'.repeat(textBytes),
        offset_bytes: '0',
        total_bytes: String(textBytes),
        continuation: null,
      },
      attachments: [],
    },
  }))
  return {
    session_id: sessionId,
    items,
    projected_body_bytes: count * (128 + textBytes),
    continuation: null,
  }
}

it('rejects a contract-valid transcript page exceeding the selected item count', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json(inputPage(9))))
  await expect(readSessionTranscript(sessionId, '1', '9', null)).rejects.toThrow(
    'selected page limits',
  )
})

it('rejects a transcript page exceeding the selected byte ceiling', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json(inputPage(8, 8200))))
  await expect(readSessionTranscript(sessionId, '1', '8', null)).rejects.toThrow('65536')
})

it('rejects contradictory transcript byte accounting through the generated decoder', async () => {
  const page = inputPage(1)
  page.projected_body_bytes += 1
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json(page)))
  await expect(readSessionTranscript(sessionId, '1', '1', null)).rejects.toThrow(
    'computed 129 bytes',
  )
})

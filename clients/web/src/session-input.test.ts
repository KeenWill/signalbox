import { afterEach, expect, it, vi } from 'vitest'
import type { WebTimelineDetailContinuation } from './generated/web-contract.mjs'
import {
  followSession,
  MAX_SESSION_MESSAGE_LENGTH,
  readExtendedSessionTranscript,
  readSessionTranscript as readTranscript,
  submitSessionInput,
} from './product'

const limits = { max_timeline_detail_items: 128, max_timeline_detail_bytes: 65536 }
const readSessionTranscript = (
  sessionId: string,
  first: string,
  through: string,
  continuation: WebTimelineDetailContinuation | null,
) => readTranscript(sessionId, first, through, continuation, limits)

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
  expect(fetch.mock.calls[0]?.[1].body).toEqual(fetch.mock.calls[1]?.[1].body)
  expect(fetch.mock.calls[0]?.[1]).toMatchObject({ method: 'POST', body: JSON.stringify(input) })
})

it('uses body continuations to replace one bounded transcript region', async () => {
  const fetch = vi.fn().mockResolvedValue(
    Response.json({
      session_id: sessionId,
      items: [
        {
          ...inputPage(1).items[0],
          address: { event_sequence: '44' },
          body: {
            type: 'user_input',
            turn_id: sessionId,
            text: { text: 'x', offset_bytes: '65000', total_bytes: '65001', continuation: null },
            attachments: [],
          },
        },
      ],
      projected_body_bytes: 129,
      continuation: null,
    }),
  )
  vi.stubGlobal('fetch', fetch)
  await readSessionTranscript(sessionId, '41', '46', {
    type: 'more_body',
    body: {
      address: { event_sequence: '44' },
      field: 'input_text',
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
    cursor_field: 'input_text',
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

it.each([null, 'text/plain'])(
  'cancels a follow body with rejected media type %s',
  async (mediaType) => {
    const cancel = vi.fn()
    const response = new Response(new ReadableStream({ cancel }), {
      headers: mediaType ? { 'content-type': mediaType } : {},
    })
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValueOnce(Response.json(snapshot('41')))
        .mockResolvedValueOnce(response),
    )
    const follow = followSession(sessionId, new AbortController().signal)
    await follow.next()
    await expect(follow.next()).rejects.toThrow('NDJSON')
    expect(cancel).toHaveBeenCalledOnce()
  },
)

it('normalizes the follow media type before adopting the stream', async () => {
  const response = stream({ kind: 'snapshot', snapshot: snapshot('41') })
  response.headers.set('content-type', ' Application/X-NDJSON ; charset=utf-8 ')
  vi.stubGlobal(
    'fetch',
    vi
      .fn()
      .mockResolvedValueOnce(Response.json(snapshot('41')))
      .mockResolvedValueOnce(response),
  )
  const follow = followSession(sessionId, new AbortController().signal)
  await follow.next()
  expect((await follow.next()).value).toMatchObject({ kind: 'snapshot' })
  await follow.return(undefined)
})

it('aborts a stalled submission at its deadline without changing the retry payload', async () => {
  vi.useFakeTimers()
  const fetch = vi.fn(
    (_url: string, init: RequestInit) =>
      new Promise<Response>((_resolve, reject) => {
        init.signal?.addEventListener(
          'abort',
          () => reject(new DOMException('Deadline elapsed', 'AbortError')),
          { once: true },
        )
      }),
  )
  vi.stubGlobal('fetch', fetch)
  const input = {
    command_id: '00000000-0000-0000-0000-000000000992',
    message: 'Retain the timed-out message.',
  }
  const result = expect(submitSessionInput(sessionId, input)).rejects.toThrow()
  await vi.advanceTimersByTimeAsync(30_000)
  await result
  expect(fetch.mock.calls[0]?.[1].signal?.aborted).toBe(true)
  expect(vi.getTimerCount()).toBe(0)
  fetch.mockResolvedValueOnce(new Response(null, { status: 204 }))
  await submitSessionInput(sessionId, input)
  expect(fetch.mock.calls[1]?.[1].body).toEqual(fetch.mock.calls[0]?.[1].body)
  expect(vi.getTimerCount()).toBe(0)
})

it('rejects an oversized draft before serialization or network I/O', async () => {
  const fetch = vi.fn()
  vi.stubGlobal('fetch', fetch)
  const stringify = vi.spyOn(JSON, 'stringify')
  try {
    await expect(
      submitSessionInput(sessionId, {
        command_id: '00000000-0000-0000-0000-000000000992',
        message: 'x'.repeat(MAX_SESSION_MESSAGE_LENGTH * 100),
      }),
    ).rejects.toThrow('draft length limit')
    expect(stringify).not.toHaveBeenCalled()
    expect(fetch).not.toHaveBeenCalled()
  } finally {
    stringify.mockRestore()
  }
})

it('clamps transcript requests and response validation to advertised limits', async () => {
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(inputPage(1)))
    .mockResolvedValueOnce(Response.json(inputPage(2)))
    .mockResolvedValueOnce(Response.json(inputPage(1, 1000)))
  vi.stubGlobal('fetch', fetch)
  const advertised = { max_timeline_detail_items: 1, max_timeline_detail_bytes: 1024 }
  await readTranscript(sessionId, '1', '9', null, advertised)
  const url = new URL(fetch.mock.calls[0]?.[0], 'http://localhost')
  expect(url.searchParams.get('max_items')).toBe('1')
  expect(url.searchParams.get('max_bytes')).toBe('1024')
  await expect(readTranscript(sessionId, '1', '9', null, advertised)).rejects.toThrow(
    'selected page limits',
  )
  await expect(readTranscript(sessionId, '1', '9', null, advertised)).rejects.toThrow(
    'selected page limits',
  )
})

it('requires a continuation page to start at the requested address', async () => {
  const page = inputPage(1)
  vi.stubGlobal(
    'fetch',
    vi
      .fn()
      .mockResolvedValueOnce(Response.json(page))
      .mockResolvedValueOnce(Response.json(page))
      .mockResolvedValueOnce(Response.json(inputPage(0))),
  )
  await expect(
    readSessionTranscript(sessionId, '1', '9', {
      type: 'more_at',
      address: { event_sequence: '1' },
    }),
  ).resolves.toEqual(page)
  await expect(
    readSessionTranscript(sessionId, '1', '9', {
      type: 'more_at',
      address: { event_sequence: '2' },
    }),
  ).rejects.toThrow('continuation address')
  await expect(
    readSessionTranscript(sessionId, '1', '9', {
      type: 'more_at',
      address: { event_sequence: '1' },
    }),
  ).rejects.toThrow('continuation address')
})

it.each([
  { field: 'input_text', member_index: 0, offset_bytes: '1' },
  { field: 'model_response', member_index: 0, offset_bytes: '0' },
  { field: 'input_text', member_index: 1, offset_bytes: '0' },
] as const)('rejects mismatched body continuation %j', async (body) => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json(inputPage(1))))
  await expect(
    readSessionTranscript(sessionId, '1', '9', {
      type: 'more_body',
      body: { ...body, address: { event_sequence: '1' } },
    }),
  ).rejects.toThrow('body continuation')
})

it('reads only new transcript addresses and keeps appended text within the item bound', async () => {
  const initial = inputPage(8)
  const addition = inputPage(1)
  addition.items[0]!.address.event_sequence = '9'
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(initial))
    .mockResolvedValueOnce(Response.json(addition))
  vi.stubGlobal('fetch', fetch)
  const held = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '8' },
    null,
    limits,
    null,
  )
  const extended = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '9' },
    null,
    limits,
    held,
  )
  expect(fetch.mock.calls[1]?.[0]).toContain('first=9&through=9')
  expect(extended.page.items.map((item) => item.address.event_sequence)).toEqual([
    '2',
    '3',
    '4',
    '5',
    '6',
    '7',
    '8',
    '9',
  ])
  expect(extended.page.projected_body_bytes).toBe(8 * 129)
  expect(extended.omittedThrough).toBe('1')
  const unchanged = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '9' },
    null,
    limits,
    extended,
  )
  expect(unchanged).toBe(extended)
  const shifted = await readExtendedSessionTranscript(
    { sessionId, first: '3', through: '9' },
    null,
    limits,
    extended,
  )
  expect(shifted.page.items.map((item) => item.address.event_sequence)).toEqual([
    '3',
    '4',
    '5',
    '6',
    '7',
    '8',
    '9',
  ])
  expect(shifted.omittedThrough).toBeNull()
  expect(fetch).toHaveBeenCalledTimes(2)
})

it('loads messages beyond metadata-only detail pages within the scan budget', async () => {
  const metadata = Array.from({ length: 4 }, (_, index) => ({
    address: { event_sequence: String(index + 1) },
    kind: 'injection_settled',
    projected_body_bytes: 128,
    body: { type: 'event_fact', kind: 'injection_settled' },
  }))
  const message = inputPage(1)
  message.items = message.items.map((item) => ({ ...item, address: { event_sequence: '5' } }))
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(
      Response.json({
        session_id: sessionId,
        items: metadata,
        projected_body_bytes: 512,
        continuation: { type: 'more_at', address: { event_sequence: '5' } },
      }),
    )
    .mockResolvedValueOnce(Response.json(message))
  vi.stubGlobal('fetch', fetch)
  const result = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '5' },
    null,
    limits,
    null,
  )
  expect(result.page).toEqual(message)
  expect(fetch).toHaveBeenCalledTimes(2)
  expect(fetch.mock.calls[1]?.[0]).toContain('cursor_address=5')
})

it.each([
  { count: 8, bytes: 1024, budget: 65536, next: '9' },
  { count: 1, bytes: 128, budget: 256, next: '2' },
])(
  'preserves incremental loading after scanning $count metadata records and $bytes bytes',
  async ({ count, bytes, budget, next }) => {
    const continuation = { type: 'more_at' as const, address: { event_sequence: next } }
    const fetch = vi.fn().mockResolvedValueOnce(
      Response.json({
        session_id: sessionId,
        items: Array.from({ length: count }, (_, index) => ({
          address: { event_sequence: String(index + 1) },
          kind: 'injection_settled',
          projected_body_bytes: bytes / count,
          body: { type: 'event_fact', kind: 'injection_settled' },
        })),
        projected_body_bytes: bytes,
        continuation,
      }),
    )
    vi.stubGlobal('fetch', fetch)
    const window = { sessionId, first: '1', through: '1000000' }
    const held = await readExtendedSessionTranscript(
      window,
      null,
      { ...limits, max_timeline_detail_bytes: budget },
      null,
    )
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(held.page.items).toEqual([])
    expect(held.page.continuation).toEqual(continuation)
    const message = inputPage(1)
    message.items[0]!.address.event_sequence = next
    fetch.mockResolvedValueOnce(Response.json(message))
    const loaded = await readExtendedSessionTranscript(window, continuation, limits, held)
    expect(loaded.page).toEqual(message)
    expect(fetch).toHaveBeenCalledTimes(2)
    expect(fetch.mock.calls[1]?.[0]).toContain(`cursor_address=${next}`)
  },
)

it('bounds appended transcript bytes even when the item count is small', async () => {
  const initial = inputPage(1, 60_000)
  const addition = inputPage(1, 10_000)
  addition.items[0]!.address.event_sequence = '2'
  vi.stubGlobal(
    'fetch',
    vi
      .fn()
      .mockResolvedValueOnce(Response.json(initial))
      .mockResolvedValueOnce(Response.json(addition)),
  )
  const held = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '1' },
    null,
    limits,
    null,
  )
  const extended = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '2' },
    null,
    limits,
    held,
  )
  expect(extended.page.items).toHaveLength(1)
  expect(extended.page.items[0]?.address.event_sequence).toBe('2')
  expect(extended.page.projected_body_bytes).toBe(10_128)
})

it.each([
  { bound: 'items', reduced: { ...limits, max_timeline_detail_items: 1 } },
  { bound: 'bytes', reduced: { ...limits, max_timeline_detail_bytes: 1024 } },
])(
  'replaces held transcript text when the current $bound limit becomes smaller',
  async ({ reduced }) => {
    const textBytes = 512
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(Response.json(inputPage(2, textBytes)))
      .mockResolvedValueOnce(Response.json(inputPage(1, textBytes)))
    vi.stubGlobal('fetch', fetch)
    const window = { sessionId, first: '1', through: '2' }
    const held = await readExtendedSessionTranscript(window, null, limits, null)
    const bounded = await readExtendedSessionTranscript(window, null, reduced, held)
    expect(fetch).toHaveBeenCalledTimes(2)
    expect(bounded.page.items).toHaveLength(1)
    expect(bounded.page.projected_body_bytes).toBeLessThanOrEqual(reduced.max_timeline_detail_bytes)
  },
)

it('relays provider text and replaces the follow stream when its consumer requests resync', async () => {
  const delta = {
    kind: 'provider_text_delta',
    turn_id: sessionId,
    model_call_id: sessionId,
    part_index: 0,
    content: 'A provider fragment',
  }
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(snapshot('41')))
    .mockResolvedValueOnce(stream({ kind: 'snapshot', snapshot: snapshot('41') }, delta))
    .mockResolvedValueOnce(Response.json(snapshot('42')))
  vi.stubGlobal('fetch', fetch)
  const follow = followSession(sessionId, new AbortController().signal, () => true)
  await follow.next()
  await follow.next()
  expect((await follow.next()).value).toEqual(delta)
  expect((await follow.next()).value).toEqual({ kind: 'resync_required', cursor: '41' })
  expect((await follow.next()).value).toEqual({ kind: 'snapshot', snapshot: snapshot('42') })
  await follow.return(undefined)
  expect(fetch).toHaveBeenCalledTimes(3)
})

it('reuses a held first page with a server continuation when its window is unchanged', async () => {
  const page = {
    ...inputPage(8),
    continuation: { type: 'more_at', address: { event_sequence: '9' } },
  }
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(page))
    .mockRejectedValue(new Error('history should not be reread'))
  vi.stubGlobal('fetch', fetch)
  const window = { sessionId, first: '1', through: '9' }
  const held = await readExtendedSessionTranscript(window, null, limits, null)
  const reused = await readExtendedSessionTranscript(window, null, limits, held)
  expect(reused).toBe(held)
  expect(reused.page.continuation).toEqual(page.continuation)
  expect(fetch).toHaveBeenCalledTimes(1)
})

it('retains the paginated first page and continuation when the tail grows', async () => {
  const page = {
    ...inputPage(8),
    continuation: { type: 'more_at', address: { event_sequence: '9' } },
  }
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(Response.json(page))
    .mockRejectedValue(new Error('history should not be reread'))
  vi.stubGlobal('fetch', fetch)
  const held = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '9' },
    null,
    limits,
    null,
  )
  const extended = await readExtendedSessionTranscript(
    { sessionId, first: '1', through: '10' },
    null,
    limits,
    held,
  )
  expect(extended.page).toBe(held.page)
  expect(extended.page.continuation).toEqual(page.continuation)
  expect(extended.through).toBe('10')
  expect(fetch).toHaveBeenCalledTimes(1)
})

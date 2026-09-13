import { afterEach, expect, it, vi } from 'vitest'
import { webContractBootstrapFixture } from '../product.fixture'
import { TranscriptWindowReader } from './transcript'
import { transcriptFixture, transcriptSessionId } from './transcript.fixture'

afterEach(() => vi.unstubAllGlobals())
it('browses a hundred thousand messages with bounded reads in both directions', async () => {
  const requests: URL[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      requests.push(url)
      return Response.json(transcriptFixture(url))
    }),
  )
  const signal = new AbortController().signal
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const tail = await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  expect(tail.details).toHaveLength(8)
  expect(tail.window.items.at(-1)?.address.event_sequence).toBe('100000')
  const before = tail.window.continuation_before
  expect(before).not.toBeNull()
  const earlier = await reader.read(
    { kind: 'before', eventSequence: before?.event_sequence ?? '' },
    webContractBootstrapFixture.limits,
    signal,
  )
  expect(earlier.window.items.at(-1)?.address.event_sequence).toBe('99992')
  const after = earlier.window.continuation_after
  const later = await reader.read(
    { kind: 'after', eventSequence: after?.event_sequence ?? '' },
    webContractBootstrapFixture.limits,
    signal,
  )
  expect(later.window.items).toEqual(tail.window.items)
  expect(requests.filter((url) => url.pathname.endsWith('/timeline-detail'))).toHaveLength(24)
})

it('rejects changed immutable kinds on overlapping transcript reads', async () => {
  let conflict = false
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url)
      if (conflict && url.pathname.endsWith('/timeline')) {
        const window = payload as import('../generated/web-contract.mjs').WebSessionTimelineWindow
        return Response.json({
          ...window,
          items: window.items.map((item, index) =>
            index ? item : { ...item, kind: 'turn_completed' },
          ),
        })
      }
      return Response.json(payload)
    }),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  conflict = true
  await expect(
    reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal),
  ).rejects.toThrow('conflicting data for a retained address')
})

it('shares item and byte allowances across every event in a retained window', async () => {
  const reads: URL[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url)
      if (!url.pathname.endsWith('/timeline-detail')) return Response.json(payload)
      reads.push(url)
      const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
      const bytes = Number(url.searchParams.get('max_bytes'))
      const text = 'a'.repeat(bytes - 128)
      return Response.json({
        ...page,
        projected_body_bytes: bytes,
        items: page.items.map((item) => ({
          ...item,
          projected_body_bytes: bytes,
          body: {
            ...item.body,
            text: { text, offset_bytes: '0', total_bytes: String(text.length), continuation: null },
          },
        })),
      })
    }),
  )
  const page = await new TranscriptWindowReader(transcriptSessionId).read(
    { kind: 'latest' },
    { ...webContractBootstrapFixture.limits, max_timeline_detail_items: 4 },
    new AbortController().signal,
  )
  expect(page.window.items).toHaveLength(4)
  expect(page.details.flatMap((detail) => detail.items)).toHaveLength(4)
  expect(reads.map((url) => Number(url.searchParams.get('max_items')))).toEqual([1, 1, 1, 1])
  expect(reads.map((url) => Number(url.searchParams.get('max_bytes')))).toEqual([
    16384, 16384, 16384, 16384,
  ])
  expect(page.details.reduce((sum, detail) => sum + detail.projected_body_bytes, 0)).toBe(65536)
})

it('reserves a valid detail read for every header under small advertised budgets', async () => {
  const reads: URL[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      reads.push(url)
      return Response.json(transcriptFixture(url))
    }),
  )
  const page = await new TranscriptWindowReader(transcriptSessionId).read(
    { kind: 'latest' },
    {
      ...webContractBootstrapFixture.limits,
      max_timeline_detail_items: 1,
      max_timeline_detail_bytes: 256,
    },
    new AbortController().signal,
  )
  expect(page.window.items).toHaveLength(1)
  expect(page.details).toHaveLength(1)
  expect(page.details[0]?.items[0]?.address.event_sequence).toBe('100000')
  expect(page.window.continuation_before).toEqual({ event_sequence: '100000' })
  expect(reads.filter((url) => url.pathname.endsWith('/timeline-detail'))).toHaveLength(1)
})

it.each([
  'turn identity',
  'attachments',
  'tool request',
  'excerpt total',
  'excerpt contents',
] as const)('rejects changed %s across overlapping initial detail reads', async (changed) => {
  const { detailItems, detailPage, resultCursor } = await import('../../e2e/session-detail-fixture')
  let conflict = false
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url, 1)
      if (changed === 'tool request' && url.pathname.endsWith('/timeline')) {
        const window = payload as import('../generated/web-contract.mjs').WebSessionTimelineWindow
        const kind = 'tool_batch_transition'
        return Response.json({
          ...window,
          items: window.items.map((item) => ({
            ...item,
            kind,
            projected_structured_bytes: 64 + kind.length,
          })),
          projected_structured_bytes: 64 + kind.length,
        })
      }
      if (!url.pathname.endsWith('/timeline-detail')) return Response.json(payload)
      if (changed === 'tool request') {
        const original = detailItems[1]
        if (original?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
        const item = {
          ...original,
          address: { event_sequence: '1' },
          body: {
            ...original.body,
            tools: original.body.tools.map((tool) => ({
              ...tool,
              request_id: conflict ? '00000000-0000-0000-0000-000000000126' : tool.request_id,
            })),
          },
        }
        return Response.json(
          detailPage([item], {
            ...resultCursor,
            body: { ...resultCursor.body, address: item.address },
          }),
        )
      }
      const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
      const item = page.items[0]
      if (item?.body.type !== 'user_input') throw new Error('Input fixture missing')
      const text =
        conflict && changed === 'excerpt total'
          ? `${item.body.text.text}!`
          : conflict && changed === 'excerpt contents'
            ? `!${item.body.text.text.slice(1)}`
            : item.body.text.text
      return Response.json({
        ...page,
        projected_body_bytes: 128 + text.length,
        items: [
          {
            ...item,
            projected_body_bytes: 128 + text.length,
            body: {
              ...item.body,
              turn_id:
                conflict && changed === 'turn identity'
                  ? '00000000-0000-0000-0000-000000000126'
                  : item.body.turn_id,
              attachments:
                changed === 'attachments'
                  ? [
                      {
                        blob_id: `sha256:${'a'.repeat(64)}`,
                        length_bytes: conflict ? '5' : '4',
                        media_type: 'image/png',
                      },
                    ]
                  : [],
              text: { ...item.body.text, text, total_bytes: String(text.length) },
            },
          },
        ],
      })
    }),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  conflict = true
  await expect(
    reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal),
  ).rejects.toThrow('retained immutable facts')
  conflict = false
  await expect(
    reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal),
  ).resolves.toBeDefined()
})

it('accepts shorter initial excerpts under a smaller budget without changing immutable facts', async () => {
  let conflict = false
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url, 1)
      if (!url.pathname.endsWith('/timeline-detail')) return Response.json(payload)
      const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
      const item = page.items[0]
      if (item?.body.type !== 'user_input') throw new Error('Input fixture missing')
      const length = Number(url.searchParams.get('max_bytes')) - 128
      const cursor = {
        address: item.address,
        field: 'input_text',
        member_index: 0,
        offset_bytes: String(length),
      } as const
      return Response.json({
        ...page,
        projected_body_bytes: length + 128,
        continuation: { type: 'more_body', body: cursor },
        items: [
          {
            ...item,
            projected_body_bytes: length + 128,
            body: {
              ...item.body,
              text: {
                text: conflict
                  ? 'a'.repeat(300) + 'b' + 'a'.repeat(length - 301)
                  : 'a'.repeat(length),
                total_bytes: '1000',
                offset_bytes: '0',
                continuation: cursor,
              },
            },
          },
        ],
      })
    }),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  for (const maxBytes of [512, 256, 384]) {
    const page = await reader.read(
      { kind: 'latest' },
      { ...webContractBootstrapFixture.limits, max_timeline_detail_bytes: maxBytes },
      signal,
    )
    expect(page.details[0]?.projected_body_bytes).toBe(maxBytes)
  }
  conflict = true
  await expect(
    reader.read(
      { kind: 'latest' },
      { ...webContractBootstrapFixture.limits, max_timeline_detail_bytes: 512 },
      signal,
    ),
  ).rejects.toThrow('retained immutable facts')
})

it('bounds immutable detail facts while traversing history', async () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) =>
      Response.json(transcriptFixture(new URL(path, 'http://localhost'))),
    ),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  for (let index = 0; index < 100; index++) {
    await reader.read(
      { kind: 'before', eventSequence: String(100001 - index * 8) },
      webContractBootstrapFixture.limits,
      signal,
    )
  }
  expect(reader).toHaveProperty('facts.size', 24)
})

it('rejects changed immutable detail facts on overlapping transcript reads', async () => {
  let conflict = false
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url)
      if (conflict && url.pathname.endsWith('/timeline-detail')) {
        const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
        return Response.json({
          ...page,
          items: page.items.map((item) => ({
            ...item,
            body:
              item.body.type === 'user_input'
                ? { ...item.body, turn_id: transcriptSessionId }
                : item.body,
          })),
        })
      }
      return Response.json(payload)
    }),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  conflict = true
  await expect(
    reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal),
  ).rejects.toThrow('changed its retained immutable facts')
})

it.each([
  ['after', '100000'],
  ['before', '1'],
])('ends fixture traversal %s boundary %s without nonexistent continuations', (anchor, address) => {
  const url = new URL(`http://localhost/api/sessions/${transcriptSessionId}/timeline`)
  url.searchParams.set('anchor', anchor)
  url.searchParams.set('address', address)
  expect(transcriptFixture(url)).toMatchObject({
    items: [],
    projected_structured_bytes: 0,
    continuation_before: null,
    continuation_after: null,
  })
})

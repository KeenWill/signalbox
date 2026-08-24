import { describe, expect, it, vi } from 'vitest'
import type { WebAttentionSnapshot, WebSessionLiveSnapshot } from '../generated/web-contract.mjs'
import {
  appendCatalog,
  applyAttentionEvent,
  applyLiveEvent,
  catalogUrl,
  EMPTY_LIVE_PRESENTATION,
  HttpSessionProjectionSource,
  MAX_CATALOG_ROWS,
  MAX_LIVE_DURABLE_ITEMS,
  MAX_PROVIDER_DRAFT_PARTS,
  readBoundedJson,
  readBoundedNdjson,
  replaceCatalog,
} from './model'

const collect = async <T>(source: AsyncIterable<T>): Promise<T[]> => {
  const records: T[] = []
  for await (const record of source) records.push(record)
  return records
}

const summary = (sessionId: string, title = sessionId) => ({
  active_turn_count: '0',
  archived: false,
  current_turn_id: null,
  judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
  last_activity: { kind: 'session' as const, unix_milliseconds: '41' },
  queued_turn_count: '0',
  session_id: sessionId,
  state: 'idle' as const,
  title_summary: title,
  title_truncated: false,
})

const catalog = (
  summaries: WebAttentionSnapshot['summaries'],
  continuation: WebAttentionSnapshot['continuation'] = null,
): WebAttentionSnapshot => ({
  continuation,
  cursor: '42',
  sort: 'last_activity_descending',
  summaries,
  total: '1000',
})

const summariesFrom = (start: number, count: number) =>
  Array.from({ length: count }, (_, index) => summary(`s-${start + index}`))

const catalogBeyondClientCeiling = () => {
  const summaries = summariesFrom(0, MAX_CATALOG_ROWS + 1)
  return {
    expectedLast: summaries[MAX_CATALOG_ROWS - 1],
    first: replaceCatalog(catalog(summaries.slice(0, 400))),
    second: catalog(summaries.slice(400)),
  }
}

const durablePresentation = (count: number) => {
  const events = Array.from({ length: count }, (_, index) => ({
    address: { event_sequence: String(index + 1) },
    cursor: String(index + 1),
    event_kind: 'model_call_transition' as const,
    kind: 'durable' as const,
  }))
  const initial = applyLiveEvent(
    EMPTY_LIVE_PRESENTATION,
    { kind: 'snapshot', snapshot: { ...liveSnapshot, observed_through: '0' } },
    liveSnapshot.session_id,
  )
  return {
    events,
    presentation: events.reduce(
      (current, event) => applyLiveEvent(current, event, liveSnapshot.session_id),
      initial,
    ),
  }
}

const draftPresentation = (count: number) => {
  const events = Array.from({ length: count }, (_, index) => ({
    content: 'x',
    kind: 'provider_text_delta' as const,
    model_call_id: 'call-1',
    part_index: index,
    turn_id: 'turn-1',
  }))
  return {
    events,
    presentation: events.reduce(
      (current, event) => applyLiveEvent(current, event, liveSnapshot.session_id),
      EMPTY_LIVE_PRESENTATION,
    ),
  }
}

const liveSnapshot: WebSessionLiveSnapshot = {
  active: {
    state: { kind: 'awaiting_tool_approval', tool_request_id: 'request-1' },
    turn_id: 'turn-1',
  },
  observed_through: '42',
  queued_turn_count: '1',
  queued_turn_ids: ['turn-2'],
  reconciliation: null,
  runner: { placement_revision: '3', runner_id: 'runner-1', state: 'pinned' },
  session_id: '00000000-0000-0000-0000-000000000001',
}

describe('session catalog projection', () => {
  it('encodes search, sort, and the exact keyset continuation', () => {
    const url = catalogUrl(
      { search: ' release train ', sort: 'last_activity_desc' },
      {
        kind: 'last_activity',
        session_id: '00000000-0000-0000-0000-000000000002',
        unix_microseconds: '123456',
      },
    )

    expect(url).toBe(
      '/api/sessions?after_session_id=00000000-0000-0000-0000-000000000002&after_activity_unix_microseconds=123456&search=release+train&sort=last_activity_desc',
    )
  })

  it('deduplicates rows repeated by adjacent pages', () => {
    const first = replaceCatalog(catalog([summary('s-1'), summary('s-2')]))
    const second = catalog([summary('s-2'), summary('s-3')])

    const result = appendCatalog(first, second)

    expect(result.summaries).toEqual([summary('s-1'), summary('s-2'), summary('s-3')])
  })

  it('retains no more than the client row ceiling', () => {
    const fixture = catalogBeyondClientCeiling()

    const result = appendCatalog(fixture.first, fixture.second)

    expect(result.summaries).toHaveLength(MAX_CATALOG_ROWS)
    expect(result.summaries.at(-1)).toBe(fixture.expectedLast)
  })

  it('uses the fleet follow stream only to update rows already admitted by the query', () => {
    const current = replaceCatalog(catalog([summary('visible', 'Old title')]))

    const result = applyAttentionEvent(current, {
      cursor: '44',
      kind: 'update',
      summaries: [summary('visible', 'Live title'), summary('outside-filter')],
    })

    expect(result.summaries).toEqual([summary('visible', 'Live title')])
  })
})

describe('session live projection', () => {
  it('replaces transient drafts on snapshot without discarding later durable headers', () => {
    const initial = applyLiveEvent(
      EMPTY_LIVE_PRESENTATION,
      { kind: 'snapshot', snapshot: liveSnapshot },
      liveSnapshot.session_id,
    )
    const withDraft = applyLiveEvent(
      initial,
      {
        content: 'partial',
        kind: 'provider_text_delta',
        model_call_id: 'call-1',
        part_index: 0,
        turn_id: 'turn-1',
      },
      liveSnapshot.session_id,
    )
    const withDurable = applyLiveEvent(
      withDraft,
      {
        address: { event_sequence: '43' },
        cursor: '43',
        event_kind: 'turn_completed',
        kind: 'durable',
      },
      liveSnapshot.session_id,
    )

    const result = applyLiveEvent(
      withDurable,
      { kind: 'snapshot', snapshot: liveSnapshot },
      liveSnapshot.session_id,
    )

    expect(result.drafts).toEqual([])
    expect(result.durable).toHaveLength(1)
    expect(result.snapshot?.active?.state.kind).toBe('awaiting_tool_approval')
  })

  it('discards all transient presentation while a lagged stream resynchronizes', () => {
    const current = applyLiveEvent(
      EMPTY_LIVE_PRESENTATION,
      {
        content: 'non-authoritative',
        kind: 'provider_text_delta',
        model_call_id: 'call-1',
        part_index: 0,
        turn_id: 'turn-1',
      },
      liveSnapshot.session_id,
    )

    const result = applyLiveEvent(
      current,
      { cursor: '42', kind: 'resync_required' },
      liveSnapshot.session_id,
    )

    expect(result.drafts).toEqual([])
    expect(result.resyncing).toBe(true)
  })

  it('bounds retained durable overlay headers', () => {
    const fixture = durablePresentation(MAX_LIVE_DURABLE_ITEMS + 1)

    expect(fixture.presentation.durable).toHaveLength(MAX_LIVE_DURABLE_ITEMS)
    expect(fixture.presentation.durable[0]).toBe(fixture.events[1])
    expect(fixture.presentation.durableGap).toBe(true)
  })

  it('rejects durable records before a snapshot or without an advancing cursor', () => {
    const durable = {
      address: { event_sequence: '43' },
      cursor: '42',
      event_kind: 'turn_completed' as const,
      kind: 'durable' as const,
    }

    expect(() => applyLiveEvent(EMPTY_LIVE_PRESENTATION, durable, liveSnapshot.session_id)).toThrow(
      'before the initial snapshot',
    )

    const initial = applyLiveEvent(
      EMPTY_LIVE_PRESENTATION,
      { kind: 'snapshot', snapshot: liveSnapshot },
      liveSnapshot.session_id,
    )
    expect(() => applyLiveEvent(initial, durable, liveSnapshot.session_id)).toThrow(
      'did not advance monotonically',
    )
  })

  it('bounds retained provider draft parts', () => {
    const fixture = draftPresentation(MAX_PROVIDER_DRAFT_PARTS + 1)

    expect(fixture.presentation.drafts).toHaveLength(MAX_PROVIDER_DRAFT_PARTS)
    expect(fixture.presentation.drafts[0]?.partIndex).toBe(fixture.events[1]?.part_index)
  })

  it('rejects a snapshot for a different selected session', () => {
    expect(() =>
      applyLiveEvent(
        EMPTY_LIVE_PRESENTATION,
        { kind: 'snapshot', snapshot: liveSnapshot },
        '00000000-0000-0000-0000-000000000002',
      ),
    ).toThrow('session live snapshot identity does not match the selected session')
  })
})

describe('bounded JSON', () => {
  it('rejects and cancels a response that exceeds the configured ceiling', async () => {
    const cancel = vi.fn()
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('{"oversized":true}'))
      },
      cancel,
    })

    await expect(readBoundedJson(new Response(body), 8)).rejects.toThrow(
      'session JSON response exceeds the byte limit',
    )
    expect(cancel).toHaveBeenCalledOnce()
  })
})

describe('catalog response correlation', () => {
  it('rejects a response whose sort differs from the request', async () => {
    const fetcher = vi.fn(async () =>
      Response.json({ ...catalog([]), sort: 'session_identity_ascending' }),
    )
    const source = new HttpSessionProjectionSource(fetcher as unknown as typeof fetch)

    await expect(source.catalogPage({ search: '', sort: 'last_activity_desc' })).rejects.toThrow(
      'does not match the requested sort',
    )
  })
})

describe('bounded NDJSON', () => {
  it('decodes records split across transport chunks', async () => {
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('{"value":'))
        controller.enqueue(new TextEncoder().encode('1}\n{"value":2}\n'))
        controller.close()
      },
    })
    const records = await collect(
      readBoundedNdjson(new Response(body), (value) => value as { value: number }),
    )

    expect(records).toEqual([{ value: 1 }, { value: 2 }])
  })

  it('rejects and cancels a record before retaining bytes beyond the configured ceiling', async () => {
    const cancel = vi.fn()
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('{"value":"oversized"}\n'))
      },
      cancel,
    })
    const body = new Response(stream)

    await expect(collect(readBoundedNdjson(body, (value) => value, 8))).rejects.toThrow(
      'NDJSON record exceeds the byte limit',
    )
    expect(cancel).toHaveBeenCalledOnce()
  })
})

import { describe, expect, it, vi } from 'vitest'
import {
  BoundedSessionHistory,
  EnormousSessionScenarioSource,
  HttpSessionTimelineSource,
  MAX_RETAINED_SESSION_ITEMS,
  MAX_TIMELINE_HTTP_RESPONSE_BYTES,
  SESSION_FOUNDATION_TOTAL,
  type SessionTimelineSource,
  TIMELINE_DETAIL_BODY_ENVELOPE_BYTES,
} from './model'

const sessionId = '00000000-0000-0000-0000-000000000991'
const timelineBootstrap = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
    bounded_session_timeline: true,
    bounded_session_timeline_detail: true,
  },
  limits: {
    max_json_body_bytes: 1024,
    max_ndjson_item_bytes: 1024,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 64 * 1024,
    max_timeline_detail_items: 128,
    max_timeline_detail_bytes: 64 * 1024,
  },
}

const detailSource = async (detail: unknown): Promise<HttpSessionTimelineSource> => {
  const request = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(new Response(JSON.stringify(timelineBootstrap)))
    .mockResolvedValueOnce(new Response(JSON.stringify(detail)))
  return HttpSessionTimelineSource.connect(request)
}

describe('BoundedSessionHistory', () => {
  it('navigates an enormous session without retaining lifetime history', async () => {
    const arbitraryAddress = '500000'
    const scenario = new EnormousSessionScenarioSource()
    const history = new BoundedSessionHistory(sessionId, scenario)
    const descriptor = await history.describe()
    const tail = await history.load(
      { kind: 'latest' },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )
    const head = await history.load(
      { kind: 'first' },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )
    const arbitrary = await history.load(
      { kind: 'around', eventSequence: arbitraryAddress },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )
    const another = await history.load(
      { kind: 'around', eventSequence: '250000' },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )

    expect(descriptor.sizes.item_count).toBe(String(SESSION_FOUNDATION_TOTAL))
    expect(tail.items.at(-1)?.address).toEqual(descriptor.latest_address)
    expect(head.items[0]?.address).toEqual(descriptor.first_address)
    expect(arbitrary.items.some((item) => item.address.event_sequence === arbitraryAddress)).toBe(
      true,
    )
    expect(another.items.some((item) => item.address.event_sequence === '250000')).toBe(true)
    expect(history.retained.length).toBeLessThanOrEqual(MAX_RETAINED_SESSION_ITEMS)
  })

  it('rejects an address that JavaScript cannot interpret losslessly as decimal', async () => {
    const history = new BoundedSessionHistory(sessionId, new EnormousSessionScenarioSource())

    await expect(
      history.load(
        { kind: 'around', eventSequence: 'timeline:12' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('unsigned decimal')
  })

  it('rejects a descriptor fact beyond the unsigned 64-bit contract', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({
        ...descriptor,
        sizes: { ...descriptor.sizes, item_count: '18446744073709551616' },
      }),
      readWindow: scenario.readWindow.bind(scenario),
    }
    const history = new BoundedSessionHistory(sessionId, source)

    await expect(history.describe()).rejects.toThrow('exceeds 64 bits')
  })

  it('compares canonical UUID identities', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({ ...descriptor, session_id: sessionId.toUpperCase() }),
      readWindow: scenario.readWindow.bind(scenario),
    }

    await expect(new BoundedSessionHistory(sessionId, source).describe()).resolves.toBeDefined()
  })

  it('canonicalizes every UUID form accepted by the server', async () => {
    const scenario = new EnormousSessionScenarioSource()

    await expect(
      new BoundedSessionHistory(`urn:uuid:${sessionId}`, scenario).describe(),
    ).resolves.toBeDefined()
    await expect(
      new BoundedSessionHistory(`{${sessionId}}`, scenario).describe(),
    ).resolves.toBeDefined()
  })

  it('rejects UUID spellings outside the server grammar', () => {
    const scenario = new EnormousSessionScenarioSource()

    expect(() => new BoundedSessionHistory(`{${sessionId.replaceAll('-', '')}}`, scenario)).toThrow(
      'session id must be a UUID',
    )
    expect(
      () => new BoundedSessionHistory('00000000-00000000-0000-0000-000000000991', scenario),
    ).toThrow('session id must be a UUID')
    expect(() => new BoundedSessionHistory(`URN:UUID:${sessionId}`, scenario)).toThrow(
      'session id must be a UUID',
    )
  })

  it('rejects contradictory descriptor boundaries', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({
        ...descriptor,
        first_address: { event_sequence: '200' },
        latest_address: { event_sequence: '100' },
      }),
      readWindow: scenario.readWindow.bind(scenario),
    }

    await expect(new BoundedSessionHistory(sessionId, source).describe()).rejects.toThrow(
      'boundaries are contradictory',
    )
  })

  it('rejects a descriptor count larger than its address span', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({
        ...descriptor,
        sizes: { ...descriptor.sizes, item_count: '2' },
        first_address: { event_sequence: '100' },
        latest_address: { event_sequence: '100' },
      }),
      readWindow: scenario.readWindow.bind(scenario),
    }

    await expect(new BoundedSessionHistory(sessionId, source).describe()).rejects.toThrow(
      'boundaries are contradictory',
    )
  })

  it('normalizes non-finite limits to their safe minima', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await new BoundedSessionHistory(sessionId, scenario).load(
      { kind: 'first' },
      { maxItems: Number.NaN, maxBytes: Number.POSITIVE_INFINITY },
    )

    expect(window.items).toHaveLength(1)
    expect(window.projected_structured_bytes).toBeLessThanOrEqual(256)
  })

  it('caps non-HTTP source limits at the protocol maxima', async () => {
    const scenario = new EnormousSessionScenarioSource()
    let receivedMaxItems = 0
    let receivedMaxBytes = 0
    const source: SessionTimelineSource = {
      limits: {
        max_timeline_window_items: 1_000_000,
        max_timeline_window_bytes: 1_000_000_000,
        max_timeline_detail_items: 128,
        max_timeline_detail_bytes: 64 * 1024,
      },
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (requestedSessionId, anchor, limits) => {
        receivedMaxItems = limits.maxItems
        receivedMaxBytes = limits.maxBytes
        return scenario.readWindow(requestedSessionId, anchor, limits)
      },
    }
    const window = await new BoundedSessionHistory(sessionId, source).load(
      { kind: 'first' },
      { maxItems: 1_000_000, maxBytes: 1_000_000_000 },
    )

    expect(receivedMaxItems).toBe(256)
    expect(receivedMaxBytes).toBe(64 * 1024)
    expect(window.items).toHaveLength(256)
  })

  it('sanitizes non-finite source ceilings to the protocol maxima', async () => {
    const scenario = new EnormousSessionScenarioSource()
    let receivedMaxItems = 0
    let receivedMaxBytes = 0
    const source: SessionTimelineSource = {
      limits: {
        max_timeline_window_items: Number.NaN,
        max_timeline_window_bytes: Number.POSITIVE_INFINITY,
        max_timeline_detail_items: 128,
        max_timeline_detail_bytes: 64 * 1024,
      },
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (requestedSessionId, anchor, limits) => {
        receivedMaxItems = limits.maxItems
        receivedMaxBytes = limits.maxBytes
        return scenario.readWindow(requestedSessionId, anchor, limits)
      },
    }

    await new BoundedSessionHistory(sessionId, source).load(
      { kind: 'first' },
      { maxItems: 1_000_000, maxBytes: 1_000_000_000 },
    )

    expect(receivedMaxItems).toBe(256)
    expect(receivedMaxBytes).toBe(64 * 1024)
  })

  it('preserves immutable bounds when a source mutates its request limits', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (requestedSessionId, anchor, limits) => {
        limits.maxItems = Number.POSITIVE_INFINITY
        limits.maxBytes = Number.POSITIVE_INFINITY
        return scenario.readWindow(requestedSessionId, anchor, {
          maxItems: 2,
          maxBytes: 256,
        })
      },
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('requested item ceiling')
  })

  it('preserves an immutable anchor when a source mutates its request anchor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (_requestedSessionId, anchor) => {
        Object.assign(anchor, { kind: 'around', eventSequence: '100' })
        return {
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: '100' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: 78,
          continuation_before: null,
          continuation_after: null,
        }
      },
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'after', eventSequence: '500' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('strictly after')
  })

  it('budgets scenario windows and descriptors from exact event-kind charges', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'first' },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 256 },
    )

    expect(descriptor.sizes.projected_structured_bytes).toBe('80800000')
    expect(window.items.map((item) => item.address.event_sequence)).toEqual(['1', '2', '3'])
    expect(window.projected_structured_bytes).toBe(248)
  })

  it('does not fabricate continuations for an empty scenario window', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'after', eventSequence: String(SESSION_FOUNDATION_TOTAL) },
      { maxItems: 1, maxBytes: 256 },
    )

    expect(window.items).toEqual([])
    expect(window.continuation_before).toBeNull()
    expect(window.continuation_after).toBeNull()
  })

  it('stops a scenario before window strictly before its anchor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'before', eventSequence: '5' },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )

    expect(window.items.map((item) => item.address.event_sequence)).toEqual(['1', '2', '3', '4'])
  })

  it('clamps a scenario before anchor beyond the latest address', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'before', eventSequence: String(SESSION_FOUNDATION_TOTAL + 1_000) },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )

    expect(window.items).toHaveLength(scenario.limits.max_timeline_window_items)
    expect(window.items.at(-1)?.address.event_sequence).toBe(String(SESSION_FOUNDATION_TOTAL))
  })

  it('fills a clipped scenario around window at the latest address', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'around', eventSequence: String(SESSION_FOUNDATION_TOTAL) },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )

    expect(window.items).toHaveLength(scenario.limits.max_timeline_window_items)
    expect(window.items.at(-1)?.address.event_sequence).toBe(String(SESSION_FOUNDATION_TOTAL))
  })

  it('clamps a scenario around anchor beyond the latest address', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await scenario.readWindow(
      sessionId,
      { kind: 'around', eventSequence: String(SESSION_FOUNDATION_TOTAL + 1_000) },
      { maxItems: scenario.limits.max_timeline_window_items, maxBytes: 64 * 1024 },
    )

    expect(window.items).toHaveLength(scenario.limits.max_timeline_window_items)
    expect(window.items.at(-1)?.address.event_sequence).toBe(String(SESSION_FOUNDATION_TOTAL))
  })

  it('rejects a bootstrap without bounded timeline capability', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: false,
            bounded_session_timeline_detail: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
            max_timeline_detail_items: 128,
            max_timeline_detail_bytes: 64 * 1024,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline capability is unavailable',
    )
  })

  it('rejects impossible advertised timeline ceilings', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
            bounded_session_timeline_detail: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 0,
            max_timeline_window_bytes: 255,
            max_timeline_detail_items: 128,
            max_timeline_detail_bytes: 64 * 1024,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline limits are invalid',
    )
  })

  it('rejects an advertised detail item ceiling below one', async () => {
    const invalidDetailRequest = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
            bounded_session_timeline_detail: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
            max_timeline_detail_items: 0,
            max_timeline_detail_bytes: 64 * 1024,
          },
        }),
      )

    await expect(HttpSessionTimelineSource.connect(invalidDetailRequest)).rejects.toThrow(
      'timeline detail limits are invalid',
    )
  })

  it('rejects an advertised detail byte ceiling below 256', async () => {
    const invalidDetailRequest = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
            bounded_session_timeline_detail: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
            max_timeline_detail_items: 128,
            max_timeline_detail_bytes: 255,
          },
        }),
      )

    await expect(HttpSessionTimelineSource.connect(invalidDetailRequest)).rejects.toThrow(
      'timeline detail limits are invalid',
    )
  })

  it('rejects advertised timeline ceilings above the protocol maxima', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
            bounded_session_timeline_detail: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 257,
            max_timeline_window_bytes: 64 * 1024 + 1,
            max_timeline_detail_items: 128,
            max_timeline_detail_bytes: 64 * 1024,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline limits are invalid',
    )
  })

  it('does not expose mutable retained items', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const history = new BoundedSessionHistory(sessionId, scenario)
    await history.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 })
    const retained = [...history.retained]
    const retainedItem = retained[0] as { address: { event_sequence: string } }

    retained.splice(0)
    retainedItem.address.event_sequence = '999'

    expect(history.retained).toHaveLength(1)
    expect(history.retained[0]?.address.event_sequence).toBe('1')
  })

  it('does not expose its cached descriptor to mutation', async () => {
    const history = new BoundedSessionHistory(sessionId, new EnormousSessionScenarioSource())
    const described = await history.describe()
    const mutableDescribed = described as unknown as {
      first_address: { event_sequence: string }
    }

    mutableDescribed.first_address.event_sequence = '999'
    const cached = history.descriptor
    expect(cached).toBeDefined()
    const mutableCached = cached as unknown as { latest_address: { event_sequence: string } }
    mutableCached.latest_address.event_sequence = '999'

    expect(history.descriptor?.first_address.event_sequence).toBe('1')
    expect(history.descriptor?.latest_address.event_sequence).toBe(String(SESSION_FOUNDATION_TOTAL))
  })

  it('rejects a timeline window whose addresses decrease', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '2' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
          {
            address: { event_sequence: '1' },
            kind: 'turn_activated',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 156,
        continuation_before: null,
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 2, maxBytes: 256 },
      ),
    ).rejects.toThrow('strictly increasing')
  })

  it('rejects a timeline window whose byte total understates its items', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '1' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
          {
            address: { event_sequence: '2' },
            kind: 'turn_activated',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 0,
        continuation_before: null,
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 2, maxBytes: 256 },
      ),
    ).rejects.toThrow('byte total does not match')
  })

  it('rejects an item whose projected byte charge contradicts its event kind', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '1' },
            kind: 'input_accepted',
            projected_structured_bytes: 0,
          },
        ],
        projected_structured_bytes: 0,
        continuation_before: null,
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('byte charge does not match')
  })

  it('rejects a continuation that is not a returned boundary', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '100' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
          {
            address: { event_sequence: '200' },
            kind: 'turn_activated',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 156,
        continuation_before: { event_sequence: '100' },
        continuation_after: { event_sequence: '999' },
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'around', eventSequence: '150' },
        { maxItems: 2, maxBytes: 256 },
      ),
    ).rejects.toThrow('returned boundary')
  })

  it('rejects a first window with a continuation before it', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '100' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: { event_sequence: '100' },
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('cannot continue before')
  })

  it('rejects a latest window with a continuation after it', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '100' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: { event_sequence: '100' },
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'latest' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('cannot continue after')
  })

  it('rejects first and latest windows that contradict the descriptor snapshot', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const firstSource: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => descriptor,
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '2' },
            kind: 'turn_activated',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: null,
      }),
    }
    const firstHistory = new BoundedSessionHistory(sessionId, firstSource)
    await firstHistory.describe()
    await expect(
      firstHistory.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('descriptor boundary')

    const latestSource: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => descriptor,
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '999999' },
            kind: 'turn_completed',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: null,
      }),
    }
    const latestHistory = new BoundedSessionHistory(sessionId, latestSource)
    await latestHistory.describe()
    await expect(
      latestHistory.load({ kind: 'latest' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('regressed behind the descriptor boundary')
  })

  it('rejects a window on the wrong side of a strict anchor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '100' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
          {
            address: { event_sequence: '200' },
            kind: 'turn_activated',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 156,
        continuation_before: null,
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'after', eventSequence: '500' },
        { maxItems: 2, maxBytes: 256 },
      ),
    ).rejects.toThrow('strictly after')
  })

  it('rejects an empty window for an anchor that must select an item', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => ({
        session_id: sessionId,
        items: [],
        projected_structured_bytes: 0,
        continuation_before: null,
        continuation_after: null,
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'around', eventSequence: '1' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow('requires a nonempty window')
  })

  it('bounds an HTTP timeline response before JSON decoding', async () => {
    const request = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            contract: { name: 'signalbox.web-http', version: '1' },
            capabilities: {
              bounded_json: true,
              same_origin_json_mutations: true,
              ndjson_streaming: true,
              bounded_session_timeline: true,
              bounded_session_timeline_detail: true,
            },
            limits: {
              max_json_body_bytes: 1024,
              max_ndjson_item_bytes: 1024,
              max_timeline_window_items: 256,
              max_timeline_window_bytes: 64 * 1024,
              max_timeline_detail_items: 128,
              max_timeline_detail_bytes: 64 * 1024,
            },
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      )
      .mockResolvedValueOnce(
        new Response(' '.repeat(MAX_TIMELINE_HTTP_RESPONSE_BYTES + 1), { status: 200 }),
      )
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readWindow(sessionId, { kind: 'first' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('encoded byte ceiling')
  })

  it('rejects invalid UTF-8 before JSON decoding', async () => {
    const prefix = new TextEncoder().encode(
      '{"error":{"kind":"application","code":"projection_failed","message":"',
    )
    const suffix = new TextEncoder().encode('"}}')
    const body = new Uint8Array(prefix.byteLength + 1 + suffix.byteLength)
    body.set(prefix)
    body[prefix.byteLength] = 0xff
    body.set(suffix, prefix.byteLength + 1)
    const request = async () => new Response(body, { status: 500 })

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow()
  })

  it('decodes structured API errors before throwing', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          error: { kind: 'application', code: 'projection_failed', message: 'projection failed' },
        }),
        { status: 500, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toMatchObject({
      name: 'SessionTimelineClientError',
      message: 'projection failed',
      response: {
        error: { kind: 'application', code: 'projection_failed', message: 'projection failed' },
      },
    })
  })

  it('reads typed item detail and carries an explicit body continuation', async () => {
    const detailAddress = '41'
    const detailLimits = { maxItems: 1, maxBytes: 1024 }
    const bodyContinuation = {
      type: 'more_body',
      body: {
        address: { event_sequence: detailAddress },
        field: 'input_text',
        member_index: 0,
        offset_bytes: '5',
      },
    } as const
    const firstPageFixture = {
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: detailAddress },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: {
              text: 'hello',
              offset_bytes: '0',
              total_bytes: '11',
              continuation: bodyContinuation.body,
            },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: bodyContinuation,
    } as const
    const secondPageFixture = {
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: detailAddress },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: firstPageFixture.items[0].body.turn_id,
            text: {
              text: ' world',
              offset_bytes: bodyContinuation.body.offset_bytes,
              total_bytes: firstPageFixture.items[0].body.text.total_bytes,
              continuation: null,
            },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 6,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 6,
      continuation: null,
    } as const
    const request = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            contract: { name: 'signalbox.web-http', version: '1' },
            capabilities: {
              bounded_json: true,
              same_origin_json_mutations: true,
              ndjson_streaming: true,
              bounded_session_timeline: true,
              bounded_session_timeline_detail: true,
            },
            limits: {
              max_json_body_bytes: 1024,
              max_ndjson_item_bytes: 1024,
              max_timeline_window_items: 256,
              max_timeline_window_bytes: 64 * 1024,
              max_timeline_detail_items: 128,
              max_timeline_detail_bytes: 64 * 1024,
            },
          }),
        ),
      )
      .mockResolvedValueOnce(new Response(JSON.stringify(firstPageFixture)))
      .mockResolvedValueOnce(new Response(JSON.stringify(secondPageFixture)))
    const source = await HttpSessionTimelineSource.connect(request)

    const first = await source.readItemDetail(sessionId, detailAddress, detailLimits)
    const second = await source.readItemDetail(
      sessionId,
      detailAddress,
      detailLimits,
      first.continuation ?? undefined,
    )
    const firstUrl = new URL(String(request.mock.calls[1]?.[0]), 'http://signalbox.test')
    const secondUrl = new URL(String(request.mock.calls[2]?.[0]), 'http://signalbox.test')

    expect(first).toEqual(firstPageFixture)
    expect(second).toEqual(secondPageFixture)
    expect(firstUrl.pathname).toBe(`/api/sessions/${sessionId}/timeline/${detailAddress}/detail`)
    expect(firstUrl.searchParams.get('max_items')).toBe(String(detailLimits.maxItems))
    expect(firstUrl.searchParams.get('max_bytes')).toBe(String(detailLimits.maxBytes))
    expect(secondUrl.searchParams.get('cursor_address')).toBe(
      bodyContinuation.body.address.event_sequence,
    )
    expect(secondUrl.searchParams.get('cursor_field')).toBe(bodyContinuation.body.field)
    expect(secondUrl.searchParams.get('cursor_member')).toBe(
      String(bodyContinuation.body.member_index),
    )
    expect(secondUrl.searchParams.get('cursor_offset')).toBe(bodyContinuation.body.offset_bytes)
  })

  it('rejects detail continuations that change the stable address', async () => {
    const bootstrap = {
      contract: { name: 'signalbox.web-http', version: '1' },
      capabilities: {
        bounded_json: true,
        same_origin_json_mutations: true,
        ndjson_streaming: true,
        bounded_session_timeline: true,
        bounded_session_timeline_detail: true,
      },
      limits: {
        max_json_body_bytes: 1024,
        max_ndjson_item_bytes: 1024,
        max_timeline_window_items: 256,
        max_timeline_window_bytes: 64 * 1024,
        max_timeline_detail_items: 128,
        max_timeline_detail_bytes: 64 * 1024,
      },
    }
    const detail = {
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '5', continuation: null },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: {
        type: 'more_body',
        body: {
          address: { event_sequence: '42' },
          field: 'input_text',
          member_index: 0,
          offset_bytes: '5',
        },
      },
    }
    const request = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(new Response(JSON.stringify(bootstrap)))
      .mockResolvedValueOnce(new Response(JSON.stringify(detail)))
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('changed the stable address')
  })

  it('rejects item-detail more-at continuations', async () => {
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '5', continuation: null },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: { type: 'more_at', address: { event_sequence: '42' } },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('cannot continue at another timeline item')
  })

  it('rejects an out-of-range model-call request context count', async () => {
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'model_call_transition',
          body: {
            type: 'model_call',
            turn_id: '00000000-0000-0000-0000-000000000041',
            model_call_id: '00000000-0000-0000-0000-000000000042',
            state: { type: 'prepared' },
            model_identity_id: 'anthropic:claude-sonnet',
            request_context_items: '18446744073709551616',
            usage: {},
            cause_code: null,
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES,
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow()
  })

  it('rejects an incomplete excerpt without a continuation', async () => {
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '11', continuation: null },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: null,
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('complete when no continuation is present')
  })

  it('computes continuation offsets from exact UTF-8 bytes', async () => {
    const continuation = {
      address: { event_sequence: '41' },
      field: 'input_text',
      member_index: 0,
      offset_bytes: '1',
    }
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'é', offset_bytes: '0', total_bytes: '4', continuation },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
      continuation: { type: 'more_body', body: continuation },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('the byte immediately after the excerpt')
  })

  it('rejects a continuation field that is impossible for its body variant', async () => {
    const excerptContinuation = {
      address: { event_sequence: '41' },
      field: 'input_text',
      member_index: 0,
      offset_bytes: '5',
    }
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: {
              text: 'hello',
              offset_bytes: '0',
              total_bytes: '11',
              continuation: excerptContinuation,
            },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: {
        type: 'more_body',
        body: { ...excerptContinuation, field: 'model_response' },
      },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('the excerpt body continuation')
  })

  it('accepts a canonical tool continuation from arguments to result', async () => {
    const continuation = {
      address: { event_sequence: '41' },
      field: 'tool_result',
      member_index: 0,
      offset_bytes: '0',
    } as const
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000041',
            producing_model_call_id: '00000000-0000-0000-0000-000000000141',
            state: {
              type: 'proposed',
              frontier_id: '00000000-0000-0000-0000-000000000341',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000241',
                tool_name: 'workspace_read',
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                arguments: {
                  text: '{}',
                  offset_bytes: '0',
                  total_bytes: '2',
                  continuation: null,
                },
                evidence: { type: 'request_only' },
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
      continuation: { type: 'more_body', body: continuation },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).resolves.toMatchObject({ continuation: { type: 'more_body', body: continuation } })
  })

  it('accepts a canonical continuation to the next goal member', async () => {
    const cursor = {
      type: 'more_body',
      body: {
        address: { event_sequence: '41' },
        field: 'goal_text',
        member_index: 0,
        offset_bytes: '0',
      },
    } as const
    const continuation = {
      address: { event_sequence: '41' },
      field: 'goal_text',
      member_index: 1,
      offset_bytes: '0',
    } as const
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000041',
            producing_model_call_id: '00000000-0000-0000-0000-000000000141',
            state: {
              type: 'results_projected',
              frontier_id: '00000000-0000-0000-0000-000000000341',
            },
            tools: [],
            goal_events: [
              {
                generation: '1',
                type: 'achieved',
                text: {
                  text: 'done',
                  offset_bytes: '0',
                  total_bytes: '4',
                  continuation: null,
                },
              },
            ],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 4,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 4,
      continuation: { type: 'more_body', body: continuation },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }, cursor),
    ).resolves.toMatchObject({ continuation: { type: 'more_body', body: continuation } })
  })

  it('rejects advancing fields before the current tool excerpt completes', async () => {
    const excerptContinuation = {
      address: { event_sequence: '41' },
      field: 'tool_arguments',
      member_index: 0,
      offset_bytes: '2',
    } as const
    const pageContinuation = {
      address: { event_sequence: '41' },
      field: 'tool_result',
      member_index: 0,
      offset_bytes: '0',
    } as const
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000041',
            producing_model_call_id: '00000000-0000-0000-0000-000000000141',
            state: {
              type: 'proposed',
              frontier_id: '00000000-0000-0000-0000-000000000341',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000241',
                tool_name: 'workspace_read',
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                arguments: {
                  text: '{}',
                  offset_bytes: '0',
                  total_bytes: '4',
                  continuation: excerptContinuation,
                },
                evidence: { type: 'request_only' },
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 2,
      continuation: { type: 'more_body', body: pageContinuation },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('the excerpt body continuation')
  })

  it('rejects a same-field continuation whose excerpt restarts before the request cursor', async () => {
    const cursor = {
      type: 'more_body',
      body: {
        address: { event_sequence: '41' },
        field: 'input_text',
        member_index: 0,
        offset_bytes: '10',
      },
    } as const
    const continuation = { ...cursor.body, offset_bytes: '5' }
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '11', continuation },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: { type: 'more_body', body: continuation },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }, cursor),
    ).rejects.toThrow('regressed from its request cursor')
  })

  it('rejects an incomplete excerpt without a matching page continuation', async () => {
    const continuation = {
      address: { event_sequence: '41' },
      field: 'input_text',
      member_index: 0,
      offset_bytes: '5',
    }
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '11', continuation },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: null,
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('the excerpt body continuation')
  })

  it('rejects a body-page continuation when no excerpt continues', async () => {
    const source = await detailSource({
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          body: {
            type: 'user_input',
            turn_id: '00000000-0000-0000-0000-000000000041',
            text: { text: 'hello', offset_bytes: '0', total_bytes: '5', continuation: null },
            attachments: [],
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES + 5,
      continuation: {
        type: 'more_body',
        body: {
          address: { event_sequence: '41' },
          field: 'input_text',
          member_index: 0,
          offset_bytes: '5',
        },
      },
    })

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('disagrees with its excerpt')
  })

  it('rejects a detail item below the fixed body envelope charge', async () => {
    const bootstrap = {
      contract: { name: 'signalbox.web-http', version: '1' },
      capabilities: {
        bounded_json: true,
        same_origin_json_mutations: true,
        ndjson_streaming: true,
        bounded_session_timeline: true,
        bounded_session_timeline_detail: true,
      },
      limits: {
        max_json_body_bytes: 1024,
        max_ndjson_item_bytes: 1024,
        max_timeline_window_items: 256,
        max_timeline_window_bytes: 64 * 1024,
        max_timeline_detail_items: 128,
        max_timeline_detail_bytes: 64 * 1024,
      },
    }
    const detail = {
      session_id: sessionId,
      items: [
        {
          address: { event_sequence: '41' },
          kind: 'turn_completed',
          body: {
            type: 'turn_lifecycle',
            turn_id: '00000000-0000-0000-0000-000000000041',
            lifecycle: 'terminalized',
            cause_code: 'completed',
          },
          projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES - 1,
        },
      ],
      projected_body_bytes: TIMELINE_DETAIL_BODY_ENVELOPE_BYTES - 1,
      continuation: null,
    }
    const request = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(new Response(JSON.stringify(bootstrap)))
      .mockResolvedValueOnce(new Response(JSON.stringify(detail)))
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('the computed 128 bytes')
  })

  it('fails closed when item detail capability is unavailable', async () => {
    const request = vi.fn<typeof fetch>().mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
            bounded_session_timeline_detail: false,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
            max_timeline_detail_items: 128,
            max_timeline_detail_bytes: 64 * 1024,
          },
        }),
      ),
    )
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readItemDetail(sessionId, '41', { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('detail capability is unavailable')
    expect(request).toHaveBeenCalledTimes(1)
  })
})

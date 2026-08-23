import { describe, expect, it, vi } from 'vitest'
import {
  BoundedSessionHistory,
  EnormousSessionScenarioSource,
  HttpSessionTimelineSource,
  MAX_RETAINED_SESSION_ITEMS,
  MAX_TIMELINE_HTTP_RESPONSE_BYTES,
  SESSION_FOUNDATION_TOTAL,
  type SessionTimelineSource,
} from './model'

const sessionId = '00000000-0000-0000-0000-000000000991'

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

    await expect(history.describe()).rejects.toThrow('unsigned 64-bit integer')
  })

  it('decodes custom-source descriptors at the generated boundary', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({ ...descriptor, unexpected: true }) as never,
      readWindow: scenario.readWindow.bind(scenario),
    }

    await expect(new BoundedSessionHistory(sessionId, source).describe()).rejects.toThrow()
  })

  it('orders overlapping descriptor results by observation cursor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const older = await scenario.readDescriptor(sessionId)
    const newer = {
      ...older,
      observed_through: String(SESSION_FOUNDATION_TOTAL + 38),
    }
    let resolveNewer: (descriptor: typeof newer) => void = () => undefined
    const delayedNewer = new Promise<typeof newer>((resolve) => {
      resolveNewer = resolve
    })
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: vi
        .fn<SessionTimelineSource['readDescriptor']>()
        .mockReturnValueOnce(delayedNewer)
        .mockResolvedValueOnce(older),
      readWindow: scenario.readWindow.bind(scenario),
    }
    const history = new BoundedSessionHistory(sessionId, source)

    const newerRequest = history.describe()
    await history.describe()
    resolveNewer(newer)
    await newerRequest

    expect(history.descriptor?.observed_through).toBe(newer.observed_through)
  })

  it('rejects append-only descriptor facts that regress at a higher cursor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const original = await scenario.readDescriptor(sessionId)
    const regressed = {
      ...original,
      observed_through: String(BigInt(original.observed_through) + 1n),
      sizes: {
        ...original.sizes,
        item_count: '1',
        projected_structured_bytes: '78',
      },
      first_address: { event_sequence: String(SESSION_FOUNDATION_TOTAL) },
      latest_address: { event_sequence: String(SESSION_FOUNDATION_TOTAL) },
    }
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: vi
        .fn<SessionTimelineSource['readDescriptor']>()
        .mockResolvedValueOnce(original)
        .mockResolvedValueOnce(regressed),
      readWindow: scenario.readWindow.bind(scenario),
    }
    const history = new BoundedSessionHistory(sessionId, source)
    await history.describe()

    await expect(history.describe()).rejects.toThrow('append-only facts are contradictory')
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

  it('rejects a descriptor structured total impossible for its item count', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({
        ...descriptor,
        sizes: { ...descriptor.sizes, projected_structured_bytes: '0' },
      }),
      readWindow: scenario.readWindow.bind(scenario),
    }

    await expect(new BoundedSessionHistory(sessionId, source).describe()).rejects.toThrow(
      'structured byte total is contradictory',
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
            bounded_lexical_search: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: false,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_search_query_bytes: 1,
            max_search_page_items: 1,
            max_search_snippet_bytes: 1,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline capability is unavailable',
    )
  })

  it('rejects a bootstrap without bounded JSON capability', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: false,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'bounded JSON session timeline capability is unavailable',
    )
  })

  it('rejects impossible advertised timeline ceilings', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            bounded_lexical_search: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_search_query_bytes: 1,
            max_search_page_items: 1,
            max_search_snippet_bytes: 1,
            max_timeline_window_items: 0,
            max_timeline_window_bytes: 255,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline limits are invalid',
    )
  })

  it('rejects advertised timeline ceilings above the protocol maxima', async () => {
    const request = async () =>
      new Response(
        JSON.stringify({
          contract: { name: 'signalbox.web-http', version: '1' },
          capabilities: {
            bounded_json: true,
            bounded_lexical_search: true,
            same_origin_json_mutations: true,
            ndjson_streaming: true,
            bounded_session_timeline: true,
          },
          limits: {
            max_json_body_bytes: 1024,
            max_ndjson_item_bytes: 1024,
            max_search_query_bytes: 1,
            max_search_page_items: 1,
            max_search_snippet_bytes: 1,
            max_timeline_window_items: 257,
            max_timeline_window_bytes: 64 * 1024 + 1,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'timeline limits are invalid',
    )
  })

  it('checks an HTTP response item ceiling before generated decoding', async () => {
    let requestCount = 0
    const request = async () => {
      requestCount += 1
      if (requestCount === 1) {
        return new Response(
          JSON.stringify({
            contract: { name: 'signalbox.web-http', version: '1' },
            capabilities: {
              bounded_json: true,
              same_origin_json_mutations: true,
              ndjson_streaming: true,
              bounded_session_timeline: true,
            },
            limits: {
              max_json_body_bytes: 64 * 1024,
              max_ndjson_item_bytes: 64 * 1024,
              max_timeline_window_items: 256,
              max_timeline_window_bytes: 64 * 1024,
            },
          }),
        )
      }
      return new Response(
        JSON.stringify({
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: 'invalid-before-generated-decoding' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '2' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: 156,
          continuation_before: null,
          continuation_after: null,
        }),
      )
    }
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readWindow(sessionId, { kind: 'first' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('requested item ceiling')
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

  it('checks a custom-source item ceiling before generated decoding', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const item = {
      address: { event_sequence: '1' },
      kind: 'future_event',
      projected_structured_bytes: 76,
    }
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () =>
        ({
          session_id: sessionId,
          items: Array(257).fill(item),
          projected_structured_bytes: 0,
          continuation_before: null,
          continuation_after: null,
        }) as never,
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 256, maxBytes: 64 * 1024 },
      ),
    ).rejects.toThrow('requested item ceiling')
  })

  it('requires continuations proven by a cached descriptor', async () => {
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
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: null,
      }),
    }
    const history = new BoundedSessionHistory(sessionId, source)
    await history.describe()

    await expect(history.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 })).rejects.toThrow(
      'requires a continuation after',
    )
  })

  it('rejects a window preceding the cached first address', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => ({
        ...descriptor,
        sizes: {
          ...descriptor.sizes,
          item_count: '101',
          projected_structured_bytes: '8000',
        },
        first_address: { event_sequence: '100' },
        latest_address: { event_sequence: '200' },
      }),
      readWindow: async () => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '1' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: { event_sequence: '1' },
      }),
    }
    const history = new BoundedSessionHistory(sessionId, source)
    await history.describe()

    await expect(history.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 })).rejects.toThrow(
      'precedes the cached first address',
    )
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

  it('rejects an unknown event kind from a custom source', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () =>
        ({
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: '1' },
              kind: 'future_event',
              projected_structured_bytes: 76,
            },
          ],
          projected_structured_bytes: 76,
          continuation_before: null,
          continuation_after: null,
        }) as never,
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 1, maxBytes: 256 },
      ),
    ).rejects.toThrow()
  })

  it('rejects conflicting data for a retained address', async () => {
    const scenario = new EnormousSessionScenarioSource()
    let readCount = 0
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async () => {
        readCount += 1
        return {
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: '1' },
              kind: readCount === 1 ? 'input_accepted' : 'turn_activated',
              projected_structured_bytes: 78,
            },
          ],
          projected_structured_bytes: 78,
          continuation_before: null,
          continuation_after: null,
        }
      },
    }
    const history = new BoundedSessionHistory(sessionId, source)
    await history.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 })

    await expect(
      history.load({ kind: 'around', eventSequence: '1' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('conflicting data for a retained address')
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
              bounded_lexical_search: true,
              same_origin_json_mutations: true,
              ndjson_streaming: true,
              bounded_session_timeline: true,
            },
            limits: {
              max_json_body_bytes: 1024,
              max_ndjson_item_bytes: 1024,
              max_search_query_bytes: 1,
              max_search_page_items: 1,
              max_search_snippet_bytes: 1,
              max_timeline_window_items: 256,
              max_timeline_window_bytes: 64 * 1024,
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

  it('rejects HTTP timeline responses for another session', async () => {
    const otherSessionId = '00000000-0000-0000-0000-000000000992'
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(otherSessionId)
    const window = await scenario.readWindow(
      otherSessionId,
      { kind: 'first' },
      { maxItems: 1, maxBytes: 256 },
    )
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
            },
            limits: {
              max_json_body_bytes: 1024,
              max_ndjson_item_bytes: 1024,
              max_timeline_window_items: 256,
              max_timeline_window_bytes: 64 * 1024,
            },
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      )
      .mockResolvedValueOnce(new Response(JSON.stringify(descriptor), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify(window), { status: 200 }))
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(source.readDescriptor(sessionId)).rejects.toThrow('descriptor session mismatch')
    await expect(
      source.readWindow(sessionId, { kind: 'first' }, { maxItems: 1, maxBytes: 256 }),
    ).rejects.toThrow('timeline window session mismatch')
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
})

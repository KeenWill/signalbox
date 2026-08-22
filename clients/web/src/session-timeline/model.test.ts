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

  it('normalizes non-finite limits to their safe minima', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await new BoundedSessionHistory(sessionId, scenario).load(
      { kind: 'first' },
      { maxItems: Number.NaN, maxBytes: Number.POSITIVE_INFINITY },
    )

    expect(window.items).toHaveLength(1)
    expect(window.projected_structured_bytes).toBeLessThanOrEqual(256)
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

  it('does not expose mutable retained storage', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const history = new BoundedSessionHistory(sessionId, scenario)
    await history.load({ kind: 'first' }, { maxItems: 1, maxBytes: 256 })
    const retained = [...history.retained]

    retained.splice(0)

    expect(history.retained).toHaveLength(1)
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
            projected_structured_bytes: 96,
          },
          {
            address: { event_sequence: '1' },
            kind: 'turn_activated',
            projected_structured_bytes: 96,
          },
        ],
        projected_structured_bytes: 192,
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
            projected_structured_bytes: 200,
          },
          {
            address: { event_sequence: '2' },
            kind: 'turn_activated',
            projected_structured_bytes: 200,
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
            projected_structured_bytes: 96,
          },
          {
            address: { event_sequence: '200' },
            kind: 'turn_activated',
            projected_structured_bytes: 96,
          },
        ],
        projected_structured_bytes: 192,
        continuation_before: { event_sequence: '100' },
        continuation_after: { event_sequence: '999' },
      }),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 2, maxBytes: 256 },
      ),
    ).rejects.toThrow('returned boundary')
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
            projected_structured_bytes: 96,
          },
          {
            address: { event_sequence: '200' },
            kind: 'turn_activated',
            projected_structured_bytes: 96,
          },
        ],
        projected_structured_bytes: 192,
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

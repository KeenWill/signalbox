import { describe, expect, it, vi } from 'vitest'
import { decodeWebContractBootstrap } from '../generated/web-contract.mjs'
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

  it('normalizes non-finite limits to their safe minima', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const window = await new BoundedSessionHistory(sessionId, scenario).load(
      { kind: 'first' },
      { maxItems: Number.NaN, maxBytes: Number.POSITIVE_INFINITY },
    )

    expect(window.items).toHaveLength(1)
    expect(window.projected_structured_bytes).toBeLessThanOrEqual(256)
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

  it('rejects impossible advertised detail ceilings', async () => {
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
            max_timeline_window_items: 256,
            max_timeline_window_bytes: 64 * 1024,
            max_timeline_detail_items: 0,
            max_timeline_detail_bytes: 255,
          },
        }),
      )

    await expect(HttpSessionTimelineSource.connect(request)).rejects.toThrow(
      'detail limits are invalid',
    )
  })

  it('rejects advertised detail ceilings above the protocol maxima', () => {
    const bootstrap = decodeWebContractBootstrap({
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
        max_timeline_detail_items: 129,
        max_timeline_detail_bytes: 64 * 1024 + 1,
      },
    })

    expect(() => HttpSessionTimelineSource.fromBootstrap(bootstrap)).toThrow(
      'detail limits are invalid',
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
            bounded_session_timeline_detail: false,
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

  it('correlates first and latest windows with described boundaries', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const descriptor = await scenario.readDescriptor(sessionId)
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: async () => descriptor,
      readWindow: async (_sessionId, anchor) => ({
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: anchor.kind === 'first' ? '2' : '999999' },
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
      'descriptor boundary',
    )
    await expect(history.load({ kind: 'latest' }, { maxItems: 1, maxBytes: 256 })).rejects.toThrow(
      'descriptor boundary',
    )
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
          projected_body_bytes: 133,
        },
      ],
      projected_body_bytes: 133,
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
          projected_body_bytes: 134,
        },
      ],
      projected_body_bytes: 134,
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

  it('rejects continuation disagreement on the initial detail page', async () => {
    const detailAddress = '41'
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
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
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
                    continuation: {
                      address: { event_sequence: detailAddress },
                      field: 'input_text',
                      member_index: 0,
                      offset_bytes: '5',
                    },
                  },
                  attachments: [],
                },
                projected_body_bytes: 133,
              },
            ],
            projected_body_bytes: 133,
            continuation: null,
          }),
        ),
      )
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readItemDetail(sessionId, detailAddress, { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('continuation contradicts its body excerpt')
  })

  it('rejects a text continuation that makes zero byte progress', async () => {
    const detailAddress = '41'
    const bootstrap = decodeWebContractBootstrap({
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
    })
    const continuation = {
      address: { event_sequence: detailAddress },
      field: 'input_text',
      member_index: 0,
      offset_bytes: '0',
    } as const
    const request = vi.fn<typeof fetch>().mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          session_id: sessionId,
          items: [
            {
              address: { event_sequence: detailAddress },
              kind: 'input_accepted',
              body: {
                type: 'user_input',
                turn_id: '00000000-0000-0000-0000-000000000041',
                text: {
                  text: '',
                  offset_bytes: '0',
                  total_bytes: '1',
                  continuation,
                },
                attachments: [],
              },
              projected_body_bytes: 128,
            },
          ],
          projected_body_bytes: 128,
          continuation: { type: 'more_body', body: continuation },
        }),
      ),
    )
    const source = HttpSessionTimelineSource.fromBootstrap(bootstrap, request)

    await expect(
      source.readItemDetail(sessionId, detailAddress, { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('positive byte progress')
  })

  it('rejects an initial text excerpt that does not start at byte zero', async () => {
    const detailAddress = '41'
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
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
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
                    offset_bytes: '10',
                    total_bytes: '15',
                    continuation: null,
                  },
                  attachments: [],
                },
                projected_body_bytes: 133,
              },
            ],
            projected_body_bytes: 133,
            continuation: null,
          }),
        ),
      )
    const source = await HttpSessionTimelineSource.connect(request)

    await expect(
      source.readItemDetail(sessionId, detailAddress, { maxItems: 1, maxBytes: 1024 }),
    ).rejects.toThrow('must start at byte zero')
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

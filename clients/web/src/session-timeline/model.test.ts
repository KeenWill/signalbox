import { describe, expect, it } from 'vitest'
import {
  BoundedSessionHistory,
  EnormousSessionScenarioSource,
  HttpSessionTimelineSource,
  MAX_RETAINED_SESSION_ITEMS,
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

  it('rejects continuation addresses that do not match window boundaries', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (requestedSessionId, anchor, limits) => {
        const window = await scenario.readWindow(requestedSessionId, anchor, limits)
        return { ...window, continuation_after: { event_sequence: '1' } }
      },
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'first' },
        { maxItems: 4, maxBytes: 1024 },
      ),
    ).rejects.toThrow('continuation after does not match the last item')
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

  it('rejects items on the wrong side of an addressed window anchor', async () => {
    const scenario = new EnormousSessionScenarioSource()
    const source: SessionTimelineSource = {
      limits: scenario.limits,
      readDescriptor: scenario.readDescriptor.bind(scenario),
      readWindow: async (requestedSessionId, _anchor, limits) =>
        scenario.readWindow(requestedSessionId, { kind: 'first' }, limits),
    }

    await expect(
      new BoundedSessionHistory(sessionId, source).load(
        { kind: 'after', eventSequence: '5' },
        { maxItems: 4, maxBytes: 1024 },
      ),
    ).rejects.toThrow('at or before its anchor')
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

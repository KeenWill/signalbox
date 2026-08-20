import { describe, expect, it } from 'vitest'
import {
  BoundedSessionHistory,
  EnormousSessionScenarioSource,
  MAX_RETAINED_SESSION_ITEMS,
  MAX_SESSION_WINDOW_ITEMS,
  SESSION_FOUNDATION_TOTAL,
  type SessionTimelineSource,
} from './model'

const sessionId = '00000000-0000-0000-0000-000000000991'

describe('BoundedSessionHistory', () => {
  it('navigates an enormous session without retaining lifetime history', async () => {
    const arbitraryAddress = '500000'
    const history = new BoundedSessionHistory(sessionId, new EnormousSessionScenarioSource())
    const descriptor = await history.describe()
    const tail = await history.load(
      { kind: 'latest' },
      { maxItems: MAX_SESSION_WINDOW_ITEMS, maxBytes: 64 * 1024 },
    )
    const head = await history.load(
      { kind: 'first' },
      { maxItems: MAX_SESSION_WINDOW_ITEMS, maxBytes: 64 * 1024 },
    )
    const arbitrary = await history.load(
      { kind: 'around', eventSequence: arbitraryAddress },
      { maxItems: MAX_SESSION_WINDOW_ITEMS, maxBytes: 64 * 1024 },
    )

    expect(descriptor.sizes.item_count).toBe(String(SESSION_FOUNDATION_TOTAL))
    expect(tail.items.at(-1)?.address).toEqual(descriptor.latest_address)
    expect(head.items[0]?.address).toEqual(descriptor.first_address)
    expect(arbitrary.items.some((item) => item.address.event_sequence === arbitraryAddress)).toBe(
      true,
    )
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
      readDescriptor: async () => ({
        ...descriptor,
        sizes: { ...descriptor.sizes, item_count: '18446744073709551616' },
      }),
      readWindow: scenario.readWindow.bind(scenario),
    }
    const history = new BoundedSessionHistory(sessionId, source)

    await expect(history.describe()).rejects.toThrow('exceeds 64 bits')
  })
})

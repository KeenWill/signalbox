import type {
  WebSessionTimelineDescriptor,
  WebSessionTimelineWindow,
} from '../generated/web-contract.mjs'
import {
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
} from '../generated/web-contract.mjs'

export const MAX_SESSION_WINDOW_ITEMS = 256
export const MAX_SESSION_WINDOW_BYTES = 64 * 1024
export const MAX_RETAINED_SESSION_ITEMS = 768

export type SessionWindowAnchor =
  | { kind: 'first' | 'latest' }
  | { kind: 'before' | 'after' | 'around'; eventSequence: string }

export interface SessionWindowLimits {
  maxItems: number
  maxBytes: number
}

export interface SessionTimelineSource {
  readDescriptor(sessionId: string, signal?: AbortSignal): Promise<WebSessionTimelineDescriptor>
  readWindow(
    sessionId: string,
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineWindow>
}

const MAX_U64 = (1n << 64n) - 1n

const decimalU64 = (value: string): bigint => {
  if (!/^(0|[1-9]\d*)$/.test(value)) throw new TypeError('timeline fact must be unsigned decimal')
  const parsed = BigInt(value)
  if (parsed > MAX_U64) throw new TypeError('timeline fact exceeds 64 bits')
  return parsed
}

const decimalAddress = (value: string): bigint => {
  const parsed = decimalU64(value)
  if (parsed === 0n) throw new TypeError('timeline address must be positive decimal')
  return parsed
}

const boundedLimits = (limits: SessionWindowLimits): SessionWindowLimits => ({
  maxItems: Math.min(Math.max(Math.trunc(limits.maxItems), 1), MAX_SESSION_WINDOW_ITEMS),
  maxBytes: Math.min(Math.max(Math.trunc(limits.maxBytes), 256), MAX_SESSION_WINDOW_BYTES),
})

export class HttpSessionTimelineSource implements SessionTimelineSource {
  constructor(private readonly request: typeof fetch = fetch) {}

  async readDescriptor(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineDescriptor> {
    const response = await this.request(`/api/sessions/${encodeURIComponent(sessionId)}`, {
      signal,
    })
    if (!response.ok) throw new Error(`session descriptor failed with ${response.status}`)
    return decodeWebSessionTimelineDescriptor(await response.json())
  }

  async readWindow(
    sessionId: string,
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineWindow> {
    const bounded = boundedLimits(limits)
    const query = new URLSearchParams({
      anchor: anchor.kind,
      max_items: String(bounded.maxItems),
      max_bytes: String(bounded.maxBytes),
    })
    if ('eventSequence' in anchor) query.set('address', anchor.eventSequence)
    const response = await this.request(
      `/api/sessions/${encodeURIComponent(sessionId)}/timeline?${query}`,
      { signal },
    )
    if (!response.ok) throw new Error(`session timeline failed with ${response.status}`)
    return decodeWebSessionTimelineWindow(await response.json())
  }
}

export class BoundedSessionHistory {
  private descriptorValue: WebSessionTimelineDescriptor | undefined
  private retainedValue: WebSessionTimelineWindow['items'] = []

  constructor(
    private readonly sessionId: string,
    private readonly source: SessionTimelineSource,
  ) {}

  get descriptor(): WebSessionTimelineDescriptor | undefined {
    return this.descriptorValue
  }

  get retained(): WebSessionTimelineWindow['items'] {
    return this.retainedValue
  }

  async describe(signal?: AbortSignal): Promise<WebSessionTimelineDescriptor> {
    const descriptor = await this.source.readDescriptor(this.sessionId, signal)
    if (descriptor.session_id !== this.sessionId) throw new TypeError('descriptor session mismatch')
    decimalAddress(descriptor.first_address.event_sequence)
    decimalAddress(descriptor.latest_address.event_sequence)
    decimalU64(descriptor.observed_through)
    decimalU64(descriptor.sizes.item_count)
    decimalU64(descriptor.sizes.projected_text_bytes)
    decimalU64(descriptor.sizes.projected_structured_bytes)
    decimalU64(descriptor.sizes.referenced_blob_count)
    decimalU64(descriptor.sizes.referenced_blob_bytes)
    decimalU64(descriptor.work.active_turn_count)
    decimalU64(descriptor.work.queued_turn_count)
    this.descriptorValue = descriptor
    return descriptor
  }

  async load(
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineWindow> {
    if ('eventSequence' in anchor) decimalAddress(anchor.eventSequence)
    const bounded = boundedLimits(limits)
    const window = await this.source.readWindow(this.sessionId, anchor, bounded, signal)
    if (window.session_id !== this.sessionId)
      throw new TypeError('timeline window session mismatch')
    if (window.items.length > bounded.maxItems) {
      throw new TypeError('timeline window exceeds the requested item ceiling')
    }
    if (window.projected_structured_bytes > bounded.maxBytes) {
      throw new TypeError('timeline window exceeds the requested byte ceiling')
    }
    const incoming = new Map<string, (typeof window.items)[number]>()
    for (const item of window.items) {
      const address = item.address.event_sequence
      decimalAddress(address)
      if (incoming.has(address)) throw new TypeError('timeline window repeats an address')
      incoming.set(address, item)
    }
    if (window.continuation_before) decimalAddress(window.continuation_before.event_sequence)
    if (window.continuation_after) decimalAddress(window.continuation_after.event_sequence)
    const candidates = [
      ...incoming.values(),
      ...this.retainedValue.filter((item) => !incoming.has(item.address.event_sequence)),
    ].slice(0, MAX_RETAINED_SESSION_ITEMS)
    candidates.sort((left, right) => {
      const leftAddress = decimalAddress(left.address.event_sequence)
      const rightAddress = decimalAddress(right.address.event_sequence)
      return leftAddress < rightAddress ? -1 : leftAddress > rightAddress ? 1 : 0
    })
    this.retainedValue = candidates
    return window
  }
}

const SCENARIO_SESSION_ID = '00000000-0000-0000-0000-000000000991'
export const SESSION_FOUNDATION_TOTAL = 1_000_000
const SCENARIO_ITEM_BYTES = 96

export class EnormousSessionScenarioSource implements SessionTimelineSource {
  async readDescriptor(sessionId: string): Promise<WebSessionTimelineDescriptor> {
    return decodeWebSessionTimelineDescriptor({
      session_id: sessionId,
      sizes: {
        item_count: String(SESSION_FOUNDATION_TOTAL),
        projected_text_bytes: '48000000',
        projected_structured_bytes: String(SESSION_FOUNDATION_TOTAL * SCENARIO_ITEM_BYTES),
        referenced_blob_count: '24000',
        referenced_blob_bytes: '96000000000',
      },
      first_address: { event_sequence: '1' },
      latest_address: { event_sequence: String(SESSION_FOUNDATION_TOTAL) },
      work: { active_turn_count: '1', queued_turn_count: '4' },
      observed_through: String(SESSION_FOUNDATION_TOTAL + 37),
    })
  }

  async readWindow(
    sessionId: string,
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
  ): Promise<WebSessionTimelineWindow> {
    const bounded = boundedLimits(limits)
    const count = Math.min(bounded.maxItems, Math.floor(bounded.maxBytes / SCENARIO_ITEM_BYTES))
    const addressed = 'eventSequence' in anchor ? Number(decimalAddress(anchor.eventSequence)) : 0
    const start = (() => {
      switch (anchor.kind) {
        case 'first':
          return 1
        case 'latest':
          return Math.max(SESSION_FOUNDATION_TOTAL - count + 1, 1)
        case 'after':
          return Math.min(addressed + 1, SESSION_FOUNDATION_TOTAL + 1)
        case 'before':
          return Math.max(addressed - count, 1)
        case 'around':
          return Math.max(Math.min(addressed - Math.floor(count / 2), SESSION_FOUNDATION_TOTAL), 1)
      }
    })()
    const end = Math.min(start + count - 1, SESSION_FOUNDATION_TOTAL)
    const items =
      start > end
        ? []
        : Array.from({ length: end - start + 1 }, (_, offset) => {
            const sequence = start + offset
            const kinds = [
              'input_accepted',
              'turn_activated',
              'model_call_transition',
              'tool_batch_transition',
              'turn_completed',
            ] as const
            return {
              address: { event_sequence: String(sequence) },
              kind: kinds[sequence % kinds.length],
              projected_structured_bytes: SCENARIO_ITEM_BYTES,
            }
          })
    return decodeWebSessionTimelineWindow({
      session_id: sessionId,
      items,
      projected_structured_bytes: items.length * SCENARIO_ITEM_BYTES,
      continuation_before: start > 1 ? { event_sequence: String(start) } : null,
      continuation_after: end < SESSION_FOUNDATION_TOTAL ? { event_sequence: String(end) } : null,
    })
  }
}

export const sessionFoundationScenario = async (after: string | undefined, limit: number) => {
  const history = new BoundedSessionHistory(
    SCENARIO_SESSION_ID,
    new EnormousSessionScenarioSource(),
  )
  const descriptor = await history.describe()
  const eventSequence = after?.match(/^timeline:([1-9]\d*)$/)?.[1]
  const window = await history.load(
    eventSequence ? { kind: 'after', eventSequence } : { kind: 'latest' },
    { maxItems: limit, maxBytes: MAX_SESSION_WINDOW_BYTES },
  )
  return { descriptor, window, retained: history.retained.length }
}

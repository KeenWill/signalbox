import type {
  WebApiErrorResponse,
  WebContractBootstrap,
  WebSessionTimelineDescriptor,
  WebSessionTimelineWindow,
} from '../generated/web-contract.mjs'
import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
} from '../generated/web-contract.mjs'

export const MAX_RETAINED_SESSION_ITEMS = 768

type TimelineContractLimits = Pick<
  WebContractBootstrap['limits'],
  'max_timeline_window_items' | 'max_timeline_window_bytes'
>

export type SessionWindowAnchor =
  | { kind: 'first' | 'latest' }
  | { kind: 'before' | 'after' | 'around'; eventSequence: string }

export interface SessionWindowLimits {
  maxItems: number
  maxBytes: number
}

export interface SessionTimelineSource {
  readonly limits: TimelineContractLimits
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

const boundedLimit = (value: number, minimum: number, maximum: number): number =>
  Number.isFinite(value) ? Math.min(Math.max(Math.trunc(value), minimum), maximum) : minimum

const boundedLimits = (
  limits: SessionWindowLimits,
  contract: TimelineContractLimits,
): SessionWindowLimits => ({
  maxItems: boundedLimit(limits.maxItems, 1, contract.max_timeline_window_items),
  maxBytes: boundedLimit(limits.maxBytes, 256, contract.max_timeline_window_bytes),
})

const canonicalSessionId = (value: string): string => {
  const compact = value.replaceAll('-', '').toLowerCase()
  if (!/^[0-9a-f]{32}$/.test(compact)) throw new TypeError('session id must be a UUID')
  return `${compact.slice(0, 8)}-${compact.slice(8, 12)}-${compact.slice(12, 16)}-${compact.slice(16, 20)}-${compact.slice(20)}`
}

export class SessionTimelineClientError extends Error {
  constructor(readonly response: WebApiErrorResponse) {
    super(response.error.message)
    this.name = 'SessionTimelineClientError'
  }
}

const throwApiError = async (response: Response): Promise<never> => {
  throw new SessionTimelineClientError(decodeWebApiErrorResponse(await response.json()))
}

export class HttpSessionTimelineSource implements SessionTimelineSource {
  private constructor(
    readonly limits: TimelineContractLimits,
    private readonly request: typeof fetch,
  ) {}

  static async connect(request: typeof fetch = fetch): Promise<HttpSessionTimelineSource> {
    const response = await request('/api/bootstrap')
    if (!response.ok) return throwApiError(response)
    const bootstrap = decodeWebContractBootstrap(await response.json())
    return new HttpSessionTimelineSource(bootstrap.limits, request)
  }

  async readDescriptor(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineDescriptor> {
    const response = await this.request(`/api/sessions/${encodeURIComponent(sessionId)}`, {
      signal,
    })
    if (!response.ok) return throwApiError(response)
    return decodeWebSessionTimelineDescriptor(await response.json())
  }

  async readWindow(
    sessionId: string,
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineWindow> {
    const bounded = boundedLimits(limits, this.limits)
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
    if (!response.ok) return throwApiError(response)
    return decodeWebSessionTimelineWindow(await response.json())
  }
}

export class BoundedSessionHistory {
  private descriptorValue: WebSessionTimelineDescriptor | undefined
  private retainedValue: WebSessionTimelineWindow['items'] = []
  private readonly sessionId: string

  constructor(
    sessionId: string,
    private readonly source: SessionTimelineSource,
  ) {
    this.sessionId = canonicalSessionId(sessionId)
  }

  get descriptor(): WebSessionTimelineDescriptor | undefined {
    return this.descriptorValue
  }

  get retained(): WebSessionTimelineWindow['items'] {
    return this.retainedValue
  }

  async describe(signal?: AbortSignal): Promise<WebSessionTimelineDescriptor> {
    const descriptor = await this.source.readDescriptor(this.sessionId, signal)
    if (canonicalSessionId(descriptor.session_id) !== this.sessionId) {
      throw new TypeError('descriptor session mismatch')
    }
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
    const bounded = boundedLimits(limits, this.source.limits)
    const window = await this.source.readWindow(this.sessionId, anchor, bounded, signal)
    if (canonicalSessionId(window.session_id) !== this.sessionId)
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
    const firstAddress = window.items[0]?.address.event_sequence
    const lastAddress = window.items.at(-1)?.address.event_sequence
    if (window.continuation_before) {
      decimalAddress(window.continuation_before.event_sequence)
      if (window.continuation_before.event_sequence !== firstAddress) {
        throw new TypeError('timeline continuation before does not match the first item')
      }
    }
    if (window.continuation_after) {
      decimalAddress(window.continuation_after.event_sequence)
      if (window.continuation_after.event_sequence !== lastAddress) {
        throw new TypeError('timeline continuation after does not match the last item')
      }
    }
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
const SCENARIO_TIMELINE_LIMITS: TimelineContractLimits = {
  max_timeline_window_items: 256,
  max_timeline_window_bytes: 64 * 1024,
}

export class EnormousSessionScenarioSource implements SessionTimelineSource {
  readonly limits = SCENARIO_TIMELINE_LIMITS

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
    const bounded = boundedLimits(limits, this.limits)
    const count = Math.min(bounded.maxItems, Math.floor(bounded.maxBytes / SCENARIO_ITEM_BYTES))
    const addressed = 'eventSequence' in anchor ? Number(decimalAddress(anchor.eventSequence)) : 0
    const initialStart = (() => {
      switch (anchor.kind) {
        case 'first':
          return 1
        case 'latest':
          return Math.max(SESSION_FOUNDATION_TOTAL - count + 1, 1)
        case 'after':
          return Math.min(addressed + 1, SESSION_FOUNDATION_TOTAL + 1)
        case 'before':
          return 1
        case 'around':
          return Math.max(Math.min(addressed - Math.floor(count / 2), SESSION_FOUNDATION_TOTAL), 1)
      }
    })()
    const end =
      anchor.kind === 'before'
        ? Math.min(addressed - 1, SESSION_FOUNDATION_TOTAL)
        : Math.min(initialStart + count - 1, SESSION_FOUNDATION_TOTAL)
    const start = anchor.kind === 'before' ? Math.max(end - count + 1, 1) : initialStart
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
    const firstItem = items[0]
    const lastItem = items.at(-1)
    return decodeWebSessionTimelineWindow({
      session_id: sessionId,
      items,
      projected_structured_bytes: items.length * SCENARIO_ITEM_BYTES,
      continuation_before:
        firstItem && start > 1 ? { event_sequence: firstItem.address.event_sequence } : null,
      continuation_after:
        lastItem && end < SESSION_FOUNDATION_TOTAL
          ? { event_sequence: lastItem.address.event_sequence }
          : null,
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
    { maxItems: limit, maxBytes: SCENARIO_TIMELINE_LIMITS.max_timeline_window_bytes },
  )
  return { descriptor, window, retained: history.retained.length }
}

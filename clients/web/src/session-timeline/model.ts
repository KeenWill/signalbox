import type {
  WebApiErrorResponse,
  WebContractBootstrap,
  WebSessionTimelineDescriptor,
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from '../generated/web-contract.mjs'
import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineDetailPage,
  decodeWebSessionTimelineWindow,
} from '../generated/web-contract.mjs'

export const MAX_RETAINED_SESSION_ITEMS = 768
// Hard safety ceiling preventing a regressed endpoint from materializing an
// unbounded JSON response before the generated decoder can reject its shape.
export const MAX_TIMELINE_HTTP_RESPONSE_BYTES = 256 * 1024

type TimelineContractLimits = Pick<
  WebContractBootstrap['limits'],
  | 'max_timeline_window_items'
  | 'max_timeline_window_bytes'
  | 'max_timeline_detail_items'
  | 'max_timeline_detail_bytes'
>

type TimelineDetailCursor = NonNullable<WebSessionTimelineDetailPage['continuation']>

export type SessionWindowAnchor =
  | { kind: 'first' | 'latest' }
  | { kind: 'before' | 'after' | 'around'; eventSequence: string }

export interface SessionWindowLimits {
  maxItems: number
  maxBytes: number
}

export interface SessionDetailLimits {
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

const boundedDetailLimits = (
  limits: SessionDetailLimits,
  contract: TimelineContractLimits,
): SessionDetailLimits => ({
  maxItems: boundedLimit(limits.maxItems, 1, contract.max_timeline_detail_items),
  maxBytes: boundedLimit(limits.maxBytes, 256, contract.max_timeline_detail_bytes),
})

const canonicalSessionId = (value: string): string => {
  const simple = /^[0-9a-f]{32}$/i
  const hyphenated = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
  const unwrapped =
    value.startsWith('urn:uuid:') && hyphenated.test(value.slice('urn:uuid:'.length))
      ? value.slice('urn:uuid:'.length)
      : value.startsWith('{') && value.endsWith('}') && hyphenated.test(value.slice(1, -1))
        ? value.slice(1, -1)
        : simple.test(value) || hyphenated.test(value)
          ? value
          : ''
  const compact = unwrapped.toLowerCase().replaceAll('-', '')
  if (!/^[0-9a-f]{32}$/.test(compact)) throw new TypeError('session id must be a UUID')
  return `${compact.slice(0, 8)}-${compact.slice(8, 12)}-${compact.slice(12, 16)}-${compact.slice(16, 20)}-${compact.slice(20)}`
}

const readBoundedJson = async (response: Response): Promise<unknown> => {
  const reader = response.body?.getReader()
  if (!reader) throw new TypeError('HTTP response has no body')
  const chunks: Uint8Array[] = []
  let byteCount = 0
  while (true) {
    const next = await reader.read()
    if (next.done) break
    byteCount += next.value.byteLength
    if (byteCount > MAX_TIMELINE_HTTP_RESPONSE_BYTES) {
      await reader.cancel()
      throw new TypeError('timeline HTTP response exceeds its encoded byte ceiling')
    }
    chunks.push(next.value)
  }
  const encoded = new Uint8Array(byteCount)
  let offset = 0
  for (const chunk of chunks) {
    encoded.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(encoded)) as unknown
}

const cloneTimelineItem = (
  item: WebSessionTimelineWindow['items'][number],
): WebSessionTimelineWindow['items'][number] => ({
  ...item,
  address: { ...item.address },
})

export class SessionTimelineClientError extends Error {
  constructor(readonly response: WebApiErrorResponse) {
    super(response.error.message)
    this.name = 'SessionTimelineClientError'
  }
}

const throwApiError = async (response: Response): Promise<never> => {
  throw new SessionTimelineClientError(decodeWebApiErrorResponse(await readBoundedJson(response)))
}

export class HttpSessionTimelineSource implements SessionTimelineSource {
  private constructor(
    readonly limits: TimelineContractLimits,
    private readonly detailAvailable: boolean,
    private readonly request: typeof fetch,
  ) {}

  static async connect(request: typeof fetch = fetch): Promise<HttpSessionTimelineSource> {
    const response = await request('/api/bootstrap')
    if (!response.ok) return throwApiError(response)
    const bootstrap = decodeWebContractBootstrap(await readBoundedJson(response))
    if (!bootstrap.capabilities.bounded_session_timeline) {
      throw new TypeError('bounded session timeline capability is unavailable')
    }
    if (
      bootstrap.limits.max_timeline_window_items < 1 ||
      bootstrap.limits.max_timeline_window_bytes < 256
    ) {
      throw new TypeError('bounded session timeline limits are invalid')
    }
    if (
      bootstrap.capabilities.bounded_session_timeline_detail &&
      (bootstrap.limits.max_timeline_detail_items < 1 ||
        bootstrap.limits.max_timeline_detail_bytes < 256)
    ) {
      throw new TypeError('bounded session timeline detail limits are invalid')
    }
    return new HttpSessionTimelineSource(
      bootstrap.limits,
      bootstrap.capabilities.bounded_session_timeline_detail,
      request,
    )
  }

  async readDescriptor(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineDescriptor> {
    const response = await this.request(`/api/sessions/${encodeURIComponent(sessionId)}`, {
      signal,
    })
    if (!response.ok) return throwApiError(response)
    return decodeWebSessionTimelineDescriptor(await readBoundedJson(response))
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
    return decodeWebSessionTimelineWindow(await readBoundedJson(response))
  }

  async readItemDetail(
    sessionId: string,
    eventSequence: string,
    limits: SessionDetailLimits,
    cursor?: TimelineDetailCursor,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineDetailPage> {
    if (!this.detailAvailable) {
      throw new TypeError('bounded session timeline detail capability is unavailable')
    }
    const address = String(decimalAddress(eventSequence))
    const bounded = boundedDetailLimits(limits, this.limits)
    const query = new URLSearchParams({
      max_items: String(bounded.maxItems),
      max_bytes: String(bounded.maxBytes),
    })
    if (cursor?.type === 'more_at') {
      query.set('cursor_address', cursor.address.event_sequence)
    }
    if (cursor?.type === 'more_body') {
      query.set('cursor_address', cursor.body.address.event_sequence)
      query.set('cursor_field', cursor.body.field)
      query.set('cursor_member', String(cursor.body.member_index))
      query.set('cursor_offset', cursor.body.offset_bytes)
    }
    const response = await this.request(
      `/api/sessions/${encodeURIComponent(sessionId)}/timeline/${address}/detail?${query}`,
      { signal },
    )
    if (!response.ok) return throwApiError(response)
    const page = decodeWebSessionTimelineDetailPage(await readBoundedJson(response))
    if (canonicalSessionId(page.session_id) !== canonicalSessionId(sessionId)) {
      throw new TypeError('timeline detail session mismatch')
    }
    if (page.items.length > bounded.maxItems) {
      throw new TypeError('timeline detail exceeds the requested item ceiling')
    }
    let projectedBodyBytes = 0
    for (const item of page.items) {
      if (item.address.event_sequence !== address) {
        throw new TypeError('item detail returned a different timeline address')
      }
      projectedBodyBytes += item.projected_body_bytes
      if (!Number.isSafeInteger(projectedBodyBytes)) {
        throw new TypeError('timeline detail byte total is not a safe integer')
      }
    }
    if (projectedBodyBytes !== page.projected_body_bytes) {
      throw new TypeError('timeline detail byte total does not match its items')
    }
    if (projectedBodyBytes > bounded.maxBytes) {
      throw new TypeError('timeline detail exceeds the requested byte ceiling')
    }
    return page
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
    return this.retainedValue.map(cloneTimelineItem)
  }

  async describe(signal?: AbortSignal): Promise<WebSessionTimelineDescriptor> {
    const descriptor = await this.source.readDescriptor(this.sessionId, signal)
    if (canonicalSessionId(descriptor.session_id) !== this.sessionId) {
      throw new TypeError('descriptor session mismatch')
    }
    const firstAddress = decimalAddress(descriptor.first_address.event_sequence)
    const latestAddress = decimalAddress(descriptor.latest_address.event_sequence)
    const observedThrough = decimalU64(descriptor.observed_through)
    const itemCount = decimalU64(descriptor.sizes.item_count)
    if (itemCount === 0n || firstAddress > latestAddress || latestAddress > observedThrough) {
      throw new TypeError('descriptor timeline boundaries are contradictory')
    }
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
    const anchorAddress =
      'eventSequence' in anchor ? decimalAddress(anchor.eventSequence) : undefined
    const bounded = boundedLimits(limits, this.source.limits)
    const window = await this.source.readWindow(this.sessionId, anchor, bounded, signal)
    if (canonicalSessionId(window.session_id) !== this.sessionId)
      throw new TypeError('timeline window session mismatch')
    if (window.items.length > bounded.maxItems) {
      throw new TypeError('timeline window exceeds the requested item ceiling')
    }
    const incoming = new Map<string, (typeof window.items)[number]>()
    let previousAddress: bigint | undefined
    let projectedStructuredBytes = 0
    for (const item of window.items) {
      const address = item.address.event_sequence
      const parsedAddress = decimalAddress(address)
      if (
        anchor.kind === 'after' &&
        anchorAddress !== undefined &&
        parsedAddress <= anchorAddress
      ) {
        throw new TypeError('timeline window item is not strictly after its anchor')
      }
      if (
        anchor.kind === 'before' &&
        anchorAddress !== undefined &&
        parsedAddress >= anchorAddress
      ) {
        throw new TypeError('timeline window item is not strictly before its anchor')
      }
      if (previousAddress !== undefined && parsedAddress <= previousAddress) {
        throw new TypeError('timeline window addresses must be strictly increasing')
      }
      if (incoming.has(address)) throw new TypeError('timeline window repeats an address')
      incoming.set(address, cloneTimelineItem(item))
      previousAddress = parsedAddress
      projectedStructuredBytes += item.projected_structured_bytes
      if (!Number.isSafeInteger(projectedStructuredBytes)) {
        throw new TypeError('timeline window byte total is not a safe integer')
      }
    }
    if (projectedStructuredBytes !== window.projected_structured_bytes) {
      throw new TypeError('timeline window byte total does not match its items')
    }
    if (projectedStructuredBytes > bounded.maxBytes) {
      throw new TypeError('timeline window exceeds the requested byte ceiling')
    }
    const firstItemAddress = window.items[0]?.address.event_sequence
    const lastItemAddress = window.items.at(-1)?.address.event_sequence
    if (anchor.kind === 'first' && window.continuation_before) {
      throw new TypeError('first timeline window cannot continue before its anchor')
    }
    if (anchor.kind === 'latest' && window.continuation_after) {
      throw new TypeError('latest timeline window cannot continue after its anchor')
    }
    if (window.continuation_before) {
      decimalAddress(window.continuation_before.event_sequence)
      if (window.continuation_before.event_sequence !== firstItemAddress) {
        throw new TypeError('timeline continuation does not match its returned boundary')
      }
    }
    if (window.continuation_after) {
      decimalAddress(window.continuation_after.event_sequence)
      if (window.continuation_after.event_sequence !== lastItemAddress) {
        throw new TypeError('timeline continuation does not match its returned boundary')
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
  max_timeline_detail_items: 128,
  max_timeline_detail_bytes: 64 * 1024,
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
    const start =
      anchor.kind === 'before' || anchor.kind === 'around'
        ? Math.max(end - count + 1, 1)
        : initialStart
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

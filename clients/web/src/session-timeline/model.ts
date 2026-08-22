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
const MAX_CONTRACT_TIMELINE_WINDOW_ITEMS = 256
const MAX_CONTRACT_TIMELINE_WINDOW_BYTES = 64 * 1024
const PROJECTED_ITEM_ENVELOPE_BYTES = 64
// Hard safety ceiling preventing a regressed endpoint from materializing an
// unbounded JSON response before the generated decoder can reject its shape.
export const MAX_TIMELINE_HTTP_RESPONSE_BYTES = 256 * 1024

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

const projectedItemBytes = (kind: string): number =>
  PROJECTED_ITEM_ENVELOPE_BYTES + new TextEncoder().encode(kind).byteLength

const boundedLimit = (value: number, minimum: number, maximum: number): number =>
  Number.isFinite(value) ? Math.min(Math.max(Math.trunc(value), minimum), maximum) : minimum

const boundedSourceLimit = (value: number, minimum: number, maximum: number): number =>
  Number.isFinite(value) ? Math.min(Math.max(Math.trunc(value), minimum), maximum) : maximum

const boundedLimits = (
  limits: SessionWindowLimits,
  contract: TimelineContractLimits,
): SessionWindowLimits => ({
  maxItems: boundedLimit(
    limits.maxItems,
    1,
    boundedSourceLimit(contract.max_timeline_window_items, 1, MAX_CONTRACT_TIMELINE_WINDOW_ITEMS),
  ),
  maxBytes: boundedLimit(
    limits.maxBytes,
    256,
    boundedSourceLimit(contract.max_timeline_window_bytes, 256, MAX_CONTRACT_TIMELINE_WINDOW_BYTES),
  ),
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

const cloneTimelineDescriptor = (
  descriptor: WebSessionTimelineDescriptor,
): WebSessionTimelineDescriptor => ({
  ...descriptor,
  sizes: { ...descriptor.sizes },
  first_address: { ...descriptor.first_address },
  latest_address: { ...descriptor.latest_address },
  work: { ...descriptor.work },
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
      bootstrap.limits.max_timeline_window_items > MAX_CONTRACT_TIMELINE_WINDOW_ITEMS ||
      bootstrap.limits.max_timeline_window_bytes < 256 ||
      bootstrap.limits.max_timeline_window_bytes > MAX_CONTRACT_TIMELINE_WINDOW_BYTES
    ) {
      throw new TypeError('bounded session timeline limits are invalid')
    }
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
    return this.descriptorValue && cloneTimelineDescriptor(this.descriptorValue)
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
    const addressSpan = latestAddress - firstAddress + 1n
    if (
      itemCount === 0n ||
      firstAddress > latestAddress ||
      latestAddress > observedThrough ||
      itemCount > addressSpan
    ) {
      throw new TypeError('descriptor timeline boundaries are contradictory')
    }
    decimalU64(descriptor.sizes.projected_text_bytes)
    decimalU64(descriptor.sizes.projected_structured_bytes)
    decimalU64(descriptor.sizes.referenced_blob_count)
    decimalU64(descriptor.sizes.referenced_blob_bytes)
    decimalU64(descriptor.work.active_turn_count)
    decimalU64(descriptor.work.queued_turn_count)
    this.descriptorValue = cloneTimelineDescriptor(descriptor)
    return cloneTimelineDescriptor(descriptor)
  }

  async load(
    anchor: SessionWindowAnchor,
    limits: SessionWindowLimits,
    signal?: AbortSignal,
  ): Promise<WebSessionTimelineWindow> {
    const anchorKind = anchor.kind
    const sourceAnchor = { ...anchor }
    const anchorAddress =
      'eventSequence' in anchor ? decimalAddress(anchor.eventSequence) : undefined
    const bounded = boundedLimits(limits, this.source.limits)
    const maxItems = bounded.maxItems
    const maxBytes = bounded.maxBytes
    const window = await this.source.readWindow(
      this.sessionId,
      sourceAnchor,
      { maxItems, maxBytes },
      signal,
    )
    if (canonicalSessionId(window.session_id) !== this.sessionId)
      throw new TypeError('timeline window session mismatch')
    if (window.items.length > maxItems) {
      throw new TypeError('timeline window exceeds the requested item ceiling')
    }
    if (
      window.items.length === 0 &&
      (anchorKind === 'first' || anchorKind === 'latest' || anchorKind === 'around')
    ) {
      throw new TypeError('timeline anchor requires a nonempty window')
    }
    const incoming = new Map<string, (typeof window.items)[number]>()
    let previousAddress: bigint | undefined
    let projectedStructuredBytes = 0
    for (const item of window.items) {
      const address = item.address.event_sequence
      const parsedAddress = decimalAddress(address)
      if (anchorKind === 'after' && anchorAddress !== undefined && parsedAddress <= anchorAddress) {
        throw new TypeError('timeline window item is not strictly after its anchor')
      }
      if (
        anchorKind === 'before' &&
        anchorAddress !== undefined &&
        parsedAddress >= anchorAddress
      ) {
        throw new TypeError('timeline window item is not strictly before its anchor')
      }
      if (previousAddress !== undefined && parsedAddress <= previousAddress) {
        throw new TypeError('timeline window addresses must be strictly increasing')
      }
      if (incoming.has(address)) throw new TypeError('timeline window repeats an address')
      if (item.projected_structured_bytes !== projectedItemBytes(item.kind)) {
        throw new TypeError('timeline item byte charge does not match its event kind')
      }
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
    if (projectedStructuredBytes > maxBytes) {
      throw new TypeError('timeline window exceeds the requested byte ceiling')
    }
    const firstItemAddress = window.items[0]?.address.event_sequence
    const lastItemAddress = window.items.at(-1)?.address.event_sequence
    if (anchorKind === 'first' && window.continuation_before) {
      throw new TypeError('first timeline window cannot continue before its anchor')
    }
    if (anchorKind === 'latest' && window.continuation_after) {
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
const SCENARIO_EVENT_KINDS = [
  'input_accepted',
  'turn_activated',
  'model_call_transition',
  'tool_batch_transition',
  'turn_completed',
] as const

const scenarioEventKind = (sequence: number): (typeof SCENARIO_EVENT_KINDS)[number] => {
  const kind = SCENARIO_EVENT_KINDS[sequence % SCENARIO_EVENT_KINDS.length]
  if (kind === undefined) throw new TypeError('timeline event kind is unavailable')
  return kind
}

const scenarioProjectedBytesThrough = (sequence: number): number => {
  const cycleBytes = SCENARIO_EVENT_KINDS.reduce(
    (total, kind) => total + projectedItemBytes(kind),
    0,
  )
  const completeCycles = Math.floor(sequence / SCENARIO_EVENT_KINDS.length)
  const remainder = sequence % SCENARIO_EVENT_KINDS.length
  let total = completeCycles * cycleBytes
  for (let offset = 1; offset <= remainder; offset += 1) {
    total += projectedItemBytes(scenarioEventKind(offset))
  }
  return total
}

const scenarioItem = (sequence: number) => {
  const kind = scenarioEventKind(sequence)
  return {
    address: { event_sequence: String(sequence) },
    kind,
    projected_structured_bytes: projectedItemBytes(kind),
  }
}

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
        projected_structured_bytes: String(scenarioProjectedBytesThrough(SESSION_FOUNDATION_TOTAL)),
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
    const addressed = 'eventSequence' in anchor ? Number(decimalAddress(anchor.eventSequence)) : 0
    const candidateSequences = (() => {
      switch (anchor.kind) {
        case 'first':
          return Array.from({ length: bounded.maxItems }, (_, offset) => offset + 1)
        case 'latest':
          return Array.from(
            { length: bounded.maxItems },
            (_, offset) => SESSION_FOUNDATION_TOTAL - offset,
          )
        case 'after':
          return Array.from({ length: bounded.maxItems }, (_, offset) => addressed + offset + 1)
        case 'before':
          return Array.from(
            { length: bounded.maxItems },
            (_, offset) => Math.min(addressed, SESSION_FOUNDATION_TOTAL + 1) - offset - 1,
          )
        case 'around': {
          const aroundAddress = Math.min(addressed, SESSION_FOUNDATION_TOTAL)
          const candidates = Array.from(
            { length: bounded.maxItems * 2 },
            (_, offset) => Math.max(aroundAddress - bounded.maxItems + 1, 1) + offset,
          ).filter((sequence) => sequence <= SESSION_FOUNDATION_TOTAL)
          candidates.sort(
            (left, right) =>
              Math.abs(left - aroundAddress) - Math.abs(right - aroundAddress) || left - right,
          )
          return candidates.slice(0, bounded.maxItems)
        }
      }
    })()
    const items = [] as ReturnType<typeof scenarioItem>[]
    let projectedBytes = 0
    for (const sequence of candidateSequences) {
      if (sequence < 1 || sequence > SESSION_FOUNDATION_TOTAL) continue
      const item = scenarioItem(sequence)
      if (projectedBytes + item.projected_structured_bytes > bounded.maxBytes) break
      projectedBytes += item.projected_structured_bytes
      items.push(item)
    }
    items.sort(
      (left, right) => Number(left.address.event_sequence) - Number(right.address.event_sequence),
    )
    const firstItem = items[0]
    const lastItem = items.at(-1)
    return decodeWebSessionTimelineWindow({
      session_id: sessionId,
      items,
      projected_structured_bytes: projectedBytes,
      continuation_before:
        firstItem && Number(firstItem.address.event_sequence) > 1
          ? { event_sequence: firstItem.address.event_sequence }
          : null,
      continuation_after:
        lastItem && Number(lastItem.address.event_sequence) < SESSION_FOUNDATION_TOTAL
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

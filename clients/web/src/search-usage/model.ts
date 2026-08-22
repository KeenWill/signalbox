import type {
  WebApiErrorResponse,
  WebContractBootstrap,
  WebSearchPage,
  WebUsageCallPage,
  WebUsageSummary,
} from '../generated/web-contract.mjs'
import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebSearchPage,
  decodeWebUsageCallPage,
  decodeWebUsageSummary,
} from '../generated/web-contract.mjs'

const MAX_CONTRACT_SEARCH_ITEMS = 100
const MAX_CONTRACT_SEARCH_QUERY_BYTES = 512
const MAX_CONTRACT_SEARCH_SNIPPET_BYTES = 512
const MAX_CONTRACT_USAGE_GROUPS = 256
const MAX_CONTRACT_USAGE_CALLS = 100
// Hard encoded-response ceiling before a generated decoder sees attacker-controlled JSON.
export const MAX_SEARCH_USAGE_HTTP_RESPONSE_BYTES = 512 * 1024

export type SearchScope = { kind: 'global' } | { kind: 'session'; sessionId: string }

export interface SearchRequest {
  text: string
  scope: SearchScope
  maxItems: number
  after?: WebSearchPage['continuation']
}

export interface UsageFilters {
  fromMicros?: string
  toMicros?: string
  sessionId?: string
  turnId?: string
  modelId?: string
  provenance?: 'reported' | 'estimated'
  callKind?: 'model_call' | 'approval_judge'
}

export interface UsageCallsRequest {
  filters: UsageFilters
  order: 'newest' | 'oldest'
  maxItems: number
  after?: WebUsageCallPage['continuation']
}

type SearchUsageLimits = Pick<
  WebContractBootstrap['limits'],
  | 'max_search_query_bytes'
  | 'max_search_page_items'
  | 'max_search_snippet_bytes'
  | 'max_usage_aggregate_groups'
  | 'max_usage_call_page_items'
>

export interface SearchUsageSource {
  readonly limits: SearchUsageLimits
  search(request: SearchRequest, signal?: AbortSignal): Promise<WebSearchPage>
  usageSummary(filters: UsageFilters, signal?: AbortSignal): Promise<WebUsageSummary>
  usageCalls(request: UsageCallsRequest, signal?: AbortSignal): Promise<WebUsageCallPage>
}

const boundedInteger = (value: number, maximum: number): number => {
  if (!Number.isFinite(value)) return 1
  return Math.min(Math.max(Math.trunc(value), 1), maximum)
}

const utf8Bytes = (value: string): Uint8Array => new TextEncoder().encode(value)

const validateSearchPage = (
  page: WebSearchPage,
  requestedItems: number,
  limits: SearchUsageLimits,
): WebSearchPage => {
  if (page.results.length > requestedItems || page.results.length > limits.max_search_page_items) {
    throw new TypeError('search response exceeds its item ceiling')
  }
  for (const result of page.results) {
    const snippet = utf8Bytes(result.snippet)
    if (snippet.byteLength > limits.max_search_snippet_bytes) {
      throw new TypeError('search response snippet exceeds its byte ceiling')
    }
    const boundaries = new Set([0])
    let byteOffset = 0
    for (const character of result.snippet) {
      byteOffset += utf8Bytes(character).byteLength
      boundaries.add(byteOffset)
    }
    let precedingEnd = 0
    for (const highlight of result.highlights) {
      if (
        !boundaries.has(highlight.start_byte) ||
        !boundaries.has(highlight.end_byte) ||
        highlight.start_byte < precedingEnd ||
        highlight.start_byte >= highlight.end_byte
      ) {
        throw new TypeError('search response contains invalid highlight bounds')
      }
      precedingEnd = highlight.end_byte
    }
  }
  return page
}

const validateUsageSummary = (
  summary: WebUsageSummary,
  limits: SearchUsageLimits,
): WebUsageSummary => {
  if (summary.groups.length > limits.max_usage_aggregate_groups) {
    throw new TypeError('usage summary exceeds its group ceiling')
  }
  return summary
}

const validateUsageCallPage = (
  page: WebUsageCallPage,
  requestedItems: number,
  limits: SearchUsageLimits,
): WebUsageCallPage => {
  if (page.calls.length > requestedItems || page.calls.length > limits.max_usage_call_page_items) {
    throw new TypeError('usage call response exceeds its item ceiling')
  }
  return page
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
    if (byteCount > MAX_SEARCH_USAGE_HTTP_RESPONSE_BYTES) {
      await reader.cancel()
      throw new TypeError('search or usage HTTP response exceeds its encoded byte ceiling')
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

export class SearchUsageClientError extends Error {
  constructor(readonly response: WebApiErrorResponse) {
    super(response.error.message)
    this.name = 'SearchUsageClientError'
  }
}

const throwApiError = async (response: Response): Promise<never> => {
  throw new SearchUsageClientError(decodeWebApiErrorResponse(await readBoundedJson(response)))
}

const appendUsageFilters = (query: URLSearchParams, filters: UsageFilters): void => {
  if (filters.fromMicros) query.set('from_micros', filters.fromMicros)
  if (filters.toMicros) query.set('to_micros', filters.toMicros)
  if (filters.sessionId) query.set('session_id', filters.sessionId)
  if (filters.turnId) query.set('turn_id', filters.turnId)
  if (filters.modelId) query.set('model_id', filters.modelId)
  if (filters.provenance) query.set('provenance', filters.provenance)
  if (filters.callKind) query.set('call_kind', filters.callKind)
}

export class HttpSearchUsageSource implements SearchUsageSource {
  private constructor(
    readonly limits: SearchUsageLimits,
    private readonly request: typeof fetch,
  ) {}

  static async connect(request: typeof fetch = fetch): Promise<HttpSearchUsageSource> {
    const response = await request('/api/bootstrap')
    if (!response.ok) return throwApiError(response)
    const bootstrap = decodeWebContractBootstrap(await readBoundedJson(response))
    if (!bootstrap.capabilities.bounded_lexical_search) {
      throw new TypeError('bounded lexical search capability is unavailable')
    }
    if (!bootstrap.capabilities.bounded_usage_cost) {
      throw new TypeError('bounded usage and cost capability is unavailable')
    }
    if (
      bootstrap.limits.max_search_query_bytes < 1 ||
      bootstrap.limits.max_search_query_bytes > MAX_CONTRACT_SEARCH_QUERY_BYTES ||
      bootstrap.limits.max_search_page_items < 1 ||
      bootstrap.limits.max_search_page_items > MAX_CONTRACT_SEARCH_ITEMS ||
      bootstrap.limits.max_search_snippet_bytes < 1 ||
      bootstrap.limits.max_search_snippet_bytes > MAX_CONTRACT_SEARCH_SNIPPET_BYTES ||
      bootstrap.limits.max_usage_aggregate_groups < 1 ||
      bootstrap.limits.max_usage_aggregate_groups > MAX_CONTRACT_USAGE_GROUPS ||
      bootstrap.limits.max_usage_call_page_items < 1 ||
      bootstrap.limits.max_usage_call_page_items > MAX_CONTRACT_USAGE_CALLS
    ) {
      throw new TypeError('search or usage contract limits are invalid')
    }
    return new HttpSearchUsageSource(bootstrap.limits, request)
  }

  async search(request: SearchRequest, signal?: AbortSignal): Promise<WebSearchPage> {
    if (utf8Bytes(request.text).byteLength > this.limits.max_search_query_bytes) {
      throw new TypeError('search expression exceeds its UTF-8 byte ceiling')
    }
    const requestedItems = boundedInteger(request.maxItems, this.limits.max_search_page_items)
    const query = new URLSearchParams({
      strategy: 'lexical',
      q: request.text,
      max_items: String(requestedItems),
    })
    if (request.scope.kind === 'session') query.set('session_id', request.scope.sessionId)
    if (request.after) {
      query.set('after_address', request.after.address.event_sequence)
      query.set('after_projection', request.after.projection_id)
    }
    const response = await this.request(`/api/search?${query}`, { signal })
    if (!response.ok) return throwApiError(response)
    return validateSearchPage(
      decodeWebSearchPage(await readBoundedJson(response)),
      requestedItems,
      this.limits,
    )
  }

  async usageSummary(filters: UsageFilters, signal?: AbortSignal): Promise<WebUsageSummary> {
    const query = new URLSearchParams()
    appendUsageFilters(query, filters)
    const response = await this.request(`/api/usage/summary?${query}`, { signal })
    if (!response.ok) return throwApiError(response)
    return validateUsageSummary(decodeWebUsageSummary(await readBoundedJson(response)), this.limits)
  }

  async usageCalls(request: UsageCallsRequest, signal?: AbortSignal): Promise<WebUsageCallPage> {
    const requestedItems = boundedInteger(request.maxItems, this.limits.max_usage_call_page_items)
    const query = new URLSearchParams({
      order: request.order,
      max_items: String(requestedItems),
    })
    appendUsageFilters(query, request.filters)
    if (request.after) {
      query.set('after_recorded_at_micros', request.after.recorded_at_micros)
      query.set('after_call_id', request.after.call_id)
    }
    const response = await this.request(`/api/usage/calls?${query}`, { signal })
    if (!response.ok) return throwApiError(response)
    return validateUsageCallPage(
      decodeWebUsageCallPage(await readBoundedJson(response)),
      requestedItems,
      this.limits,
    )
  }
}

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebContractBootstrap,
  type WebAttentionSnapshot,
  type WebContractBootstrap,
} from './generated/web-contract.mjs'

export const productRoutes = [
  { id: 'attention', label: 'Attention', description: 'Actionable work and fleet state' },
  { id: 'sessions', label: 'Sessions', description: 'Conversation activity and history' },
  { id: 'search', label: 'Search', description: 'Global and session search' },
  { id: 'activity', label: 'Activity', description: 'Repository operations and ingestion' },
  { id: 'runners', label: 'Runners', description: 'Execution fleet' },
  { id: 'reviews', label: 'Reviews', description: 'Pull request convergence' },
  { id: 'imports', label: 'Imports', description: 'Imported conversations' },
  { id: 'usage', label: 'Usage', description: 'Tokens and cost' },
  { id: 'settings', label: 'Settings', description: 'Local workspace preferences' },
] as const

export type ProductRouteId = (typeof productRoutes)[number]['id']

export interface ProductSessionState {
  q?: string
  sort?: 'activity' | 'identity'
  archived?: boolean
  afterSession?: string
  afterActivity?: string
  session?: string
}

export const readProductSessionState = (value: Record<string, unknown>): ProductSessionState => {
  const text = (key: keyof ProductSessionState) =>
    typeof value[key] === 'string' && value[key].length > 0 ? value[key] : undefined
  return {
    q: admittedSessionSearch(value.q),
    sort: value.sort === 'identity' ? 'identity' : undefined,
    archived: value.archived === true ? true : undefined,
    afterSession: text('afterSession'),
    afterActivity: text('afterActivity'),
    session: text('session'),
  }
}

export interface ProductSessionRequest {
  search?: string
  sort: 'activity' | 'identity'
  includeArchived: boolean
  afterSession?: string
  afterActivity?: string
}

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
  readSessions(request: ProductSessionRequest, signal?: AbortSignal): Promise<WebAttentionSnapshot>
}

export const MAX_SESSION_PAGE_ITEMS = 32
export const MAX_PRODUCT_HTTP_RESPONSE_BYTES = 64 * 1024
export const MAX_SESSION_SEARCH_BYTES = 1024

const admittedSessionSearch = (value: unknown) => {
  if (typeof value === 'string' && value.indexOf(String.fromCharCode(0)) !== -1) {
    return undefined
  }
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0')) return undefined
  return new TextEncoder().encode(value).byteLength <= MAX_SESSION_SEARCH_BYTES ? value : undefined
}

const readBoundedJson = async (response: Response): Promise<unknown> => {
  const reader = response.body?.getReader()
  if (!reader) throw new TypeError('product HTTP response has no body')
  const chunks: Uint8Array[] = []
  let byteCount = 0
  while (true) {
    const next = await reader.read()
    if (next.done) break
    byteCount += next.value.byteLength
    if (byteCount > MAX_PRODUCT_HTTP_RESPONSE_BYTES) {
      await reader.cancel()
      throw new TypeError('product HTTP response exceeds its encoded byte ceiling')
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

const validateSessionPage = (
  page: WebAttentionSnapshot,
  request: ProductSessionRequest,
): WebAttentionSnapshot => {
  const expectedSort =
    request.sort === 'identity' ? 'session_identity_ascending' : 'last_activity_descending'
  const expectedContinuation = request.sort === 'identity' ? 'session_identity' : 'last_activity'
  if (page.sort !== expectedSort) {
    throw new Error(`session catalog response sort ${page.sort} contradicts ${expectedSort}`)
  }
  if (page.continuation && page.continuation.kind !== expectedContinuation) {
    throw new Error(
      `session catalog continuation ${page.continuation.kind} contradicts ${expectedContinuation}`,
    )
  }
  if (page.summaries.length > MAX_SESSION_PAGE_ITEMS) {
    throw new Error(`session catalog response exceeds ${MAX_SESSION_PAGE_ITEMS} summaries`)
  }
  if (page.continuation) {
    const boundary = page.summaries.at(-1)
    if (!boundary || page.continuation.session_id !== boundary.session_id) {
      throw new Error('session catalog continuation does not match its returned boundary')
    }
    if (page.continuation.kind === 'last_activity') {
      const milliseconds = boundary.last_activity.unix_milliseconds
      const microseconds = page.continuation.unix_microseconds
      if (!/^(0|[1-9]\d*)$/.test(milliseconds) || !/^(0|[1-9]\d*)$/.test(microseconds)) {
        throw new Error('session catalog boundary activity is not canonical')
      }
      const millisecondFloor = BigInt(milliseconds) * 1000n
      const exactMicroseconds = BigInt(microseconds)
      if (exactMicroseconds < millisecondFloor || exactMicroseconds >= millisecondFloor + 1000n) {
        throw new Error('session catalog continuation does not match its returned boundary')
      }
    }
  }
  return page
}

export class ProductRequestError extends Error {
  constructor(
    readonly code: string,
    readonly kind: 'transport' | 'application',
    message: string,
  ) {
    super(message)
  }
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await fetch('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    const bootstrap = decodeWebContractBootstrap(await readBoundedJson(response))
    if (!bootstrap.capabilities.bounded_json) {
      throw new Error('bootstrap does not provide bounded JSON responses')
    }
    return bootstrap
  }

  async readSessions(
    request: ProductSessionRequest,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    if (request.search && admittedSessionSearch(request.search) === undefined) {
      throw new TypeError('session catalog search exceeds its contract bound')
    }
    const query = new URLSearchParams({
      sort: request.sort === 'identity' ? 'session_id_asc' : 'last_activity_desc',
      include_archived: String(request.includeArchived),
    })
    if (request.search) query.set('search', request.search)
    if (request.afterSession) query.set('after_session_id', request.afterSession)
    if (request.afterActivity) {
      query.set('after_activity_unix_microseconds', request.afterActivity)
    }
    const response = await fetch(`/api/sessions?${query}`, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) {
      const failure = decodeWebApiErrorResponse(await readBoundedJson(response))
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    return validateSessionPage(decodeWebAttentionSnapshot(await readBoundedJson(response)), request)
  }
}

export const productTransport = new SameOriginProductTransport()

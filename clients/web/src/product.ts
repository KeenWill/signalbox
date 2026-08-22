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
    q: text('q'),
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
    return decodeWebContractBootstrap(await response.json())
  }

  async readSessions(
    request: ProductSessionRequest,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
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
      const failure = decodeWebApiErrorResponse(await response.json())
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    return validateSessionPage(decodeWebAttentionSnapshot(await response.json()), request)
  }
}

export const productTransport = new SameOriginProductTransport()

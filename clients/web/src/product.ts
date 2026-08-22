import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebSearchPage,
  type WebContractBootstrap,
  type WebSearchPage,
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

export interface ProductSearchState {
  q?: string
  session?: string
  afterAddress?: string
  afterProjection?: string
  around?: string
}

export interface ProductSearchRequest {
  query: string
  sessionId?: string
  maxItems: number
  after?: { address: string; projectionId: string }
}

export const readProductSearchState = (value: Record<string, unknown>): ProductSearchState => {
  const text = (key: keyof ProductSearchState) =>
    typeof value[key] === 'string' && value[key].length > 0 ? value[key] : undefined
  return {
    q: text('q'),
    session: text('session'),
    afterAddress: text('afterAddress'),
    afterProjection: text('afterProjection'),
    around: text('around'),
  }
}

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
  search(request: ProductSearchRequest, signal?: AbortSignal): Promise<WebSearchPage>
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

  async search(request: ProductSearchRequest, signal?: AbortSignal): Promise<WebSearchPage> {
    const query = new URLSearchParams({
      strategy: 'lexical',
      q: request.query,
      max_items: String(request.maxItems),
    })
    if (request.sessionId) query.set('session_id', request.sessionId)
    if (request.after) {
      query.set('after_address', request.after.address)
      query.set('after_projection', request.after.projectionId)
    }
    const response = await fetch(`/api/search?${query}`, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) {
      const failure = decodeWebApiErrorResponse(await response.json())
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    return decodeWebSearchPage(await response.json())
  }
}

export const productTransport = new SameOriginProductTransport()

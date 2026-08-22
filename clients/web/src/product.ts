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
  maxSnippetBytes: number
  after?: { address: string; projectionId: string }
}

const MAX_SEARCH_RESPONSE_BYTES = 1_048_576
const MAX_BOOTSTRAP_RESPONSE_BYTES = 65_536
const MAX_SEARCH_HIGHLIGHTS_PER_RESULT = 64
const ERROR_RESPONSE_BYTES = 16_384

const readBoundedJson = async (response: Response, maximumBytes: number): Promise<unknown> => {
  const declaredLength = response.headers.get('content-length')
  if (declaredLength !== null && Number(declaredLength) > maximumBytes) {
    throw new TypeError(`response exceeds ${maximumBytes} bytes`)
  }
  const reader = response.body?.getReader()
  if (reader === undefined) {
    const text = await response.text()
    if (new TextEncoder().encode(text).byteLength > maximumBytes) {
      throw new TypeError(`response exceeds ${maximumBytes} bytes`)
    }
    return JSON.parse(text)
  }
  const chunks: Uint8Array[] = []
  let length = 0
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      length += value.byteLength
      if (length > maximumBytes) {
        await reader.cancel()
        throw new TypeError(`response exceeds ${maximumBytes} bytes`)
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  const bytes = new Uint8Array(length)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
}

const validateSearchPageBounds = (
  page: WebSearchPage,
  request: ProductSearchRequest,
): WebSearchPage => {
  if (page.results.length > request.maxItems) throw new TypeError('search page exceeds item limit')
  const encoder = new TextEncoder()
  for (const result of page.results) {
    const snippetLength = encoder.encode(result.snippet).byteLength
    if (snippetLength > request.maxSnippetBytes) {
      throw new TypeError('search result exceeds snippet limit')
    }
    if (result.highlights.length > MAX_SEARCH_HIGHLIGHTS_PER_RESULT) {
      throw new TypeError('search result exceeds highlight limit')
    }
    let previousEnd = 0
    for (const highlight of result.highlights) {
      if (
        highlight.start_byte < previousEnd ||
        highlight.end_byte < highlight.start_byte ||
        highlight.end_byte > snippetLength
      ) {
        throw new TypeError('search result carries an invalid highlight range')
      }
      previousEnd = highlight.end_byte
    }
  }
  return page
}

export const readProductSearchState = (value: Record<string, unknown>): ProductSearchState => {
  const text = (key: keyof ProductSearchState) =>
    typeof value[key] === 'string' && value[key].length > 0 ? value[key] : undefined
  const query = value.q
  const q =
    typeof query === 'string'
      ? query || undefined
      : typeof query === 'number' || typeof query === 'boolean' || query === null
        ? String(query)
        : undefined
  return {
    q,
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

export class ProductTransportError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProductTransportError'
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
    return decodeWebContractBootstrap(await readBoundedJson(response, MAX_BOOTSTRAP_RESPONSE_BYTES))
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
    let response: Response
    try {
      response = await fetch(`/api/search?${query}`, {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
        signal,
      })
    } catch (error) {
      if (error instanceof Error && error.name === 'AbortError') throw error
      throw new ProductTransportError('The search request could not reach Signalbox.')
    }
    if (!response.ok) {
      const failure = decodeWebApiErrorResponse(
        await readBoundedJson(response, ERROR_RESPONSE_BYTES),
      )
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    const responseLimit = Math.min(
      MAX_SEARCH_RESPONSE_BYTES,
      16_384 + request.maxItems * (request.maxSnippetBytes + 2_048),
    )
    const page = decodeWebSearchPage(await readBoundedJson(response, responseLimit))
    return validateSearchPageBounds(page, request)
  }
}

export const productTransport = new SameOriginProductTransport()

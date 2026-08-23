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
  sessionParameterIsValid?: false
  afterAddress?: string
  afterProjection?: string
  cursorParametersAreValid?: false
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
const MAX_SEARCH_QUERY_BYTES = 512
const MAX_SEARCH_PAGE_ITEMS = 100
const MAX_SEARCH_SNIPPET_BYTES = 512
const ERROR_RESPONSE_BYTES = 16_384
const MAX_I64 = 9_223_372_036_854_775_807n
const WEB_CONTRACT_NAME = 'signalbox.web-http'
const WEB_CONTRACT_VERSION = '1'
const isUtf8ContinuationByte = (byte: number | undefined) =>
  byte !== undefined && (byte & 0xc0) === 0x80

const canonicalUuid = (value: string): string | undefined => {
  const compact = value
    .toLowerCase()
    .replace(/^urn:uuid:/, '')
    .replace(/^\{(.*)\}$/, '$1')
    .replaceAll('-', '')
  if (!/^[0-9a-f]{32}$/.test(compact)) return undefined
  return `${compact.slice(0, 8)}-${compact.slice(8, 12)}-${compact.slice(12, 16)}-${compact.slice(16, 20)}-${compact.slice(20)}`
}

const sourceUuids = (source: WebSearchPage['results'][number]['source']): string[] => {
  switch (source.kind) {
    case 'session':
      return [source.session_id]
    case 'accepted_input':
      return [source.accepted_input_id, source.turn_id]
    case 'steering_input':
      return [source.accepted_input_id, source.source_turn_id]
    case 'turn_transcript_entry':
      return [source.semantic_entry_id, source.turn_id]
    case 'session_transcript_entry':
      return [source.semantic_entry_id]
    case 'tool_request':
      return [source.tool_request_id, source.turn_id]
    case 'tool_attempt':
      return [source.tool_attempt_id, source.turn_id]
    case 'attachment':
      return [source.attachment_id]
    case 'derived_artifact':
      return [source.artifact_id]
  }
}

const readBoundedJson = async (
  response: Response,
  maximumBytes: number,
  streamFailureMessage: string,
): Promise<unknown> => {
  const declaredLength = response.headers.get('content-length')
  if (declaredLength !== null && Number(declaredLength) > maximumBytes) {
    throw new TypeError(`response exceeds ${maximumBytes} bytes`)
  }
  const reader = response.body?.getReader()
  if (reader === undefined) {
    let text: string
    try {
      text = await response.text()
    } catch (error) {
      if (error instanceof Error && error.name === 'AbortError') throw error
      throw new ProductTransportError(streamFailureMessage)
    }
    if (new TextEncoder().encode(text).byteLength > maximumBytes) {
      throw new TypeError(`response exceeds ${maximumBytes} bytes`)
    }
    return JSON.parse(text)
  }
  const chunks: Uint8Array[] = []
  let length = 0
  try {
    while (true) {
      let read: ReadableStreamReadResult<Uint8Array>
      try {
        read = await reader.read()
      } catch (error) {
        if (error instanceof Error && error.name === 'AbortError') throw error
        throw new ProductTransportError(streamFailureMessage)
      }
      const { done, value } = read
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
  const requestedSession =
    request.sessionId === undefined ? undefined : canonicalUuid(request.sessionId)
  let previousAddress = request.after === undefined ? undefined : BigInt(request.after.address)
  let previousProjectionId =
    request.after === undefined ? undefined : BigInt(request.after.projectionId)
  let firstResult = true
  for (const result of page.results) {
    const resultSession = canonicalUuid(result.session_id)
    if (
      resultSession === undefined ||
      sourceUuids(result.source).some((identity) => canonicalUuid(identity) === undefined)
    ) {
      throw new TypeError('search result carries an invalid UUID identity')
    }
    if (
      request.sessionId !== undefined &&
      (requestedSession === undefined || resultSession !== requestedSession)
    ) {
      throw new TypeError('search result falls outside the requested session')
    }
    if (
      result.source.kind === 'session' &&
      canonicalUuid(result.source.session_id) !== resultSession
    ) {
      throw new TypeError('search result source contradicts its session')
    }
    const address = BigInt(result.address.event_sequence)
    const projectionId = BigInt(result.projection_id)
    if (
      previousAddress !== undefined &&
      (address > previousAddress ||
        (address === previousAddress &&
          previousProjectionId !== undefined &&
          projectionId >= previousProjectionId))
    ) {
      throw new TypeError(
        firstResult && request.after !== undefined
          ? 'search page does not advance past the request cursor'
          : 'search page is not ordered newest first',
      )
    }
    firstResult = false
    previousAddress = address
    previousProjectionId = projectionId
    const snippetBytes = encoder.encode(result.snippet)
    const snippetLength = snippetBytes.byteLength
    if (snippetLength > request.maxSnippetBytes) {
      throw new TypeError('search result exceeds snippet limit')
    }
    let previousEnd = 0
    for (const highlight of result.highlights) {
      if (
        highlight.start_byte < previousEnd ||
        highlight.end_byte < highlight.start_byte ||
        highlight.end_byte > snippetLength ||
        (highlight.start_byte > 0 &&
          highlight.start_byte < snippetLength &&
          isUtf8ContinuationByte(snippetBytes[highlight.start_byte])) ||
        (highlight.end_byte > 0 &&
          highlight.end_byte < snippetLength &&
          isUtf8ContinuationByte(snippetBytes[highlight.end_byte]))
      ) {
        throw new TypeError('search result carries an invalid highlight range')
      }
      previousEnd = highlight.end_byte
    }
  }
  const continuation = page.continuation
  if (continuation != null) {
    const lastResult = page.results.at(-1)
    const projectionId = continuation.projection_id
    if (
      lastResult === undefined ||
      continuation.address.event_sequence !== lastResult.address.event_sequence ||
      continuation.projection_id !== lastResult.projection_id ||
      !/^[1-9][0-9]*$/.test(projectionId) ||
      BigInt(projectionId) > MAX_I64
    ) {
      throw new TypeError('search page carries an invalid continuation')
    }
  }
  return page
}

export const readProductSearchState = (value: Record<string, unknown>): ProductSearchState => {
  const text = (key: keyof ProductSearchState) =>
    typeof value[key] === 'string' && value[key].length > 0 ? value[key] : undefined
  const cursorText = (key: 'afterAddress' | 'afterProjection') => {
    const field = value[key]
    if (typeof field === 'string') return field.length > 0 ? field : undefined
    return typeof field === 'number' && Number.isFinite(field) ? String(field) : undefined
  }
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
    ...(value.session !== undefined && typeof value.session !== 'string'
      ? { sessionParameterIsValid: false as const }
      : {}),
    afterAddress: cursorText('afterAddress'),
    afterProjection: cursorText('afterProjection'),
    ...(Array.isArray(value.afterAddress) || Array.isArray(value.afterProjection)
      ? { cursorParametersAreValid: false as const }
      : {}),
    around: text('around'),
  }
}

const validateBootstrapSearchLimits = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
  if (
    bootstrap.contract.name !== WEB_CONTRACT_NAME ||
    bootstrap.contract.version !== WEB_CONTRACT_VERSION
  ) {
    throw new TypeError('bootstrap carries an incompatible contract identity')
  }
  const { limits } = bootstrap
  if (limits.max_search_query_bytes < 1 || limits.max_search_query_bytes > MAX_SEARCH_QUERY_BYTES) {
    throw new TypeError('bootstrap carries an invalid search query limit')
  }
  if (limits.max_search_page_items < 1 || limits.max_search_page_items > MAX_SEARCH_PAGE_ITEMS) {
    throw new TypeError('bootstrap carries an invalid search page limit')
  }
  if (
    limits.max_search_snippet_bytes < 1 ||
    limits.max_search_snippet_bytes > MAX_SEARCH_SNIPPET_BYTES
  ) {
    throw new TypeError('bootstrap carries an invalid search snippet limit')
  }
  return bootstrap
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
    let response: Response
    try {
      response = await fetch('/api/bootstrap', {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
        signal,
      })
    } catch (error) {
      if (error instanceof Error && error.name === 'AbortError') throw error
      throw new ProductTransportError('The bootstrap request could not reach Signalbox.')
    }
    if (!response.ok) {
      throw new ProductTransportError(`Bootstrap request failed with status ${response.status}.`)
    }
    return validateBootstrapSearchLimits(
      decodeWebContractBootstrap(
        await readBoundedJson(
          response,
          MAX_BOOTSTRAP_RESPONSE_BYTES,
          'The bootstrap response stream was interrupted.',
        ),
      ),
    )
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
        await readBoundedJson(
          response,
          ERROR_RESPONSE_BYTES,
          'The search response stream was interrupted.',
        ),
      )
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    const page = decodeWebSearchPage(
      await readBoundedJson(
        response,
        MAX_SEARCH_RESPONSE_BYTES,
        'The search response stream was interrupted.',
      ),
    )
    return validateSearchPageBounds(page, request)
  }
}

export const productTransport = new SameOriginProductTransport()

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebBlobDescriptor,
  decodeWebContractBootstrap,
  decodeWebSearchPage,
  decodeWebSessionCatalogSnapshot,
  type WebApiErrorResponse,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebBlobDescriptor,
  type WebContractBootstrap,
  type WebSearchPage,
  type WebSessionCatalogSnapshot,
} from './generated/web-contract.mjs'

export const productRoutes = [
  { id: 'attention', label: 'Attention', description: 'Actionable work and fleet state' },
  { id: 'sessions', label: 'Sessions', description: 'Conversation activity and history' },
  { id: 'search', label: 'Search', description: 'Global and session search' },
  { id: 'runners', label: 'Runners', description: 'Execution fleet' },
  { id: 'reviews', label: 'Reviews', description: 'Pull request convergence' },
  { id: 'imports', label: 'Imports', description: 'Imported conversations' },
  { id: 'usage', label: 'Usage', description: 'Tokens and cost' },
  { id: 'settings', label: 'Settings', description: 'Local workspace preferences' },
] as const

export type ProductRouteId = (typeof productRoutes)[number]['id']

export type ProductSurfaceState =
  | { kind: 'browser-local'; authority: 'browser preferences' }
  | { kind: 'server-backed'; owningTrack: string; facts: readonly string[] }
  | {
      kind: 'committed-unimplemented'
      owningTrack: string
      facts: readonly string[]
    }

export const productSurfaceStates: Record<ProductRouteId, ProductSurfaceState> = {
  attention: {
    kind: 'server-backed',
    owningTrack: '#992 attention projections',
    facts: ['keyset attention snapshot pages', 'streamed attention projection updates'],
  },
  sessions: {
    kind: 'server-backed',
    owningTrack: '#991 session projections',
    facts: ['bounded session descriptors', 'stable-address timeline windows'],
  },
  search: {
    kind: 'committed-unimplemented',
    owningTrack: '#994 search and usage reads',
    facts: ['cross-session search reads'],
  },
  runners: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['runner discovery reads'],
  },
  reviews: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['review discovery reads'],
  },
  imports: {
    kind: 'server-backed',
    owningTrack: '#995 discovery reads',
    facts: ['keyset import catalog pages', 'bounded imported-entry windows'],
  },
  usage: {
    kind: 'committed-unimplemented',
    owningTrack: '#994 search and usage reads',
    facts: ['usage aggregation reads'],
  },
  settings: { kind: 'browser-local', authority: 'browser preferences' },
}

export const productSurfaceCacheLabel = (surface: ProductRouteId): string | null => {
  switch (productSurfaceStates[surface].kind) {
    case 'browser-local':
      return 'Local settings'
    case 'server-backed':
      return 'Bounded query'
    case 'committed-unimplemented':
      return null
  }
}

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
  readSessions(
    request: ProductSessionRequest,
    signal?: AbortSignal,
  ): Promise<WebSessionCatalogSnapshot>
  readBlobDescriptor(input: BlobDescriptorInput, signal?: AbortSignal): Promise<WebBlobDescriptor>
  readAttention(afterSessionId?: string, signal?: AbortSignal): Promise<WebAttentionSnapshot>
  followAttention(signal?: AbortSignal): AsyncIterable<WebAttentionStreamEvent>
  search(request: ProductSearchRequest, signal?: AbortSignal): Promise<WebSearchPage>
}

export interface ProductSearchState {
  q?: string
  session?: string
  sessionParameterIsValid?: false
  afterAddress?: string
  afterProjection?: string
  cursorParametersAreValid?: false
  around?: string
}

export interface ProductSessionState {
  q?: string
  sort?: 'activity' | 'identity'
  archived?: boolean
  afterSession?: string
  afterActivity?: string
  session?: string
  workspace?: boolean
}

export type ProductRouteState = ProductSearchState & Omit<ProductSessionState, 'q' | 'session'>

export interface ProductSessionRequest {
  search?: string
  sort: 'activity' | 'identity'
  includeArchived: boolean
  afterSession?: string
  afterActivity?: string
}

export interface ProductSearchRequest {
  query: string
  sessionId?: string
  maxItems: number
  maxSnippetBytes: number
  after?: { address: string; projectionId: string }
}

export interface BlobDescriptorInput {
  digest: string
  mediaType: string
  displayFilename?: string
}

export class ProductRequestError extends Error {
  readonly status: number
  readonly response: WebApiErrorResponse

  constructor(status: number, response: WebApiErrorResponse) {
    super(response.error.message)
    this.name = 'ProductRequestError'
    this.status = status
    this.response = response
  }
}

export class ProductTransportError extends Error {
  constructor(cause: unknown) {
    super('The Signalbox daemon could not be reached.', { cause })
    this.name = 'ProductTransportError'
  }
}

export class ProductContractError extends Error {
  constructor(cause: unknown) {
    super('The bootstrap response did not match the generated web contract.', { cause })
    this.name = 'ProductContractError'
  }
}

export class ProductInputError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProductInputError'
  }
}

export const MAX_PRODUCT_JSON_BYTES = 65_536
export const MAX_NDJSON_ITEM_BYTES = 65_536
export const MAX_DECLARED_MEDIA_TYPE_BYTES = 255
export const MAX_DISPLAY_FILENAME_BYTES = 1_024
// The Attention projection contract pages at 32 summaries; the byte ceilings are the shared
// product JSON and NDJSON item limits the bootstrap already pins.
export const MAX_ATTENTION_SNAPSHOT_ITEMS = 32
export const MAX_SESSION_PAGE_ITEMS = 32
export const MAX_SESSION_SEARCH_BYTES = 1_024
const MAX_SESSION_SUMMARY_SCALARS = 128
// Hard safety ceiling: bounds search-response allocation and parse work in the browser. Search
// pages carry snippets for a full page of results, so they need a wider ceiling than the shared
// product JSON limit that bounds identity-sized responses.
const MAX_SEARCH_RESPONSE_BYTES = 1_048_576
// Hard safety ceiling: rejects advertised query limits above the browser's bounded input budget.
const MAX_SEARCH_QUERY_BYTES = 512
// Hard safety ceiling: rejects advertised page sizes above the browser's bounded render budget.
const MAX_SEARCH_PAGE_ITEMS = 100
// Hard safety ceiling: rejects advertised snippets above the browser's bounded render budget.
const MAX_SEARCH_SNIPPET_BYTES = 512
// Representation fact: projection identities are positive signed 64-bit database integers.
const MAX_I64 = 9_223_372_036_854_775_807n
const isUtf8ContinuationByte = (byte: number | undefined) =>
  byte !== undefined && (byte & 0xc0) === 0x80

const MAX_UNSIGNED_64 = 18_446_744_073_709_551_615n
const SESSION_ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const CANONICAL_NONNEGATIVE_INTEGER_PATTERN = /^(0|[1-9]\d*)$/

const CATALOG_SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

const admittedSessionIdentity = (value: unknown) =>
  typeof value === 'string' && CATALOG_SESSION_ID_PATTERN.test(value) ? value : undefined

const admittedActivityCursor = (value: unknown) => {
  const cursor =
    typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? String(value) : value
  if (
    typeof cursor !== 'string' ||
    !CANONICAL_NONNEGATIVE_INTEGER_PATTERN.test(cursor) ||
    BigInt(cursor) > MAX_UNSIGNED_64
  ) {
    return undefined
  }
  return cursor
}

export const admittedSessionSearch = (value: unknown) => {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0')) return undefined
  return new TextEncoder().encode(value).byteLength <= MAX_SESSION_SEARCH_BYTES ? value : undefined
}

export const readProductSessionState = (value: Record<string, unknown>): ProductSessionState => {
  const sort = value.sort === 'identity' ? 'identity' : undefined
  const afterSession = admittedSessionIdentity(value.afterSession)
  const afterActivity = admittedActivityCursor(value.afterActivity)
  const validContinuation =
    sort === 'identity'
      ? afterSession !== undefined && value.afterActivity === undefined
      : afterSession !== undefined && afterActivity !== undefined
  return {
    q: admittedSessionSearch(value.q),
    sort,
    archived: value.archived === true ? true : undefined,
    afterSession: validContinuation ? afterSession : undefined,
    afterActivity: validContinuation && sort !== 'identity' ? afterActivity : undefined,
    session: admittedSessionIdentity(value.session),
    workspace: value.workspace === true ? true : undefined,
  }
}

const validateCursor = (cursor: string): void => {
  if (!CANONICAL_NONNEGATIVE_INTEGER_PATTERN.test(cursor) || BigInt(cursor) > MAX_UNSIGNED_64) {
    throw new TypeError('attention cursor must be a canonical unsigned 64-bit integer')
  }
}

const validatePositiveU64 = (value: string, label: string): void => {
  validateCursor(value)
  if (value === '0') throw new TypeError(`${label} must be positive`)
}

type AttentionSummary = WebAttentionSnapshot['summaries'][number]

const validateAttentionSummary = (summary: AttentionSummary): void => {
  if (!SESSION_ID_PATTERN.test(summary.session_id)) {
    throw new TypeError('attention summary session identity must be a canonical UUID')
  }
  if (summary.current_turn_id != null && !SESSION_ID_PATTERN.test(summary.current_turn_id)) {
    throw new TypeError('attention summary current-turn identity must be a canonical UUID')
  }
  if (
    summary.current_turn_id == null &&
    [
      'active',
      'queued',
      'awaiting_approval',
      'ambiguous',
      'awaiting_tool_recovery',
      'awaiting_reconciliation',
    ].includes(summary.state)
  ) {
    throw new TypeError('turn-derived attention summary must include a current-turn identity')
  }
  if (!CANONICAL_NONNEGATIVE_INTEGER_PATTERN.test(summary.last_activity.unix_milliseconds)) {
    throw new TypeError('attention activity timestamp must be a canonical nonnegative integer')
  }
  for (const count of [
    summary.judge.actionable,
    summary.judge.completed,
    summary.judge.escalated,
    summary.judge.failed,
  ]) {
    validateCursor(count)
  }
  const expectedAction = (() => {
    switch (summary.state) {
      case 'blocked':
        return summary.goal_block?.reason === 'execution_failure' && summary.action == null
          ? null
          : 'provide_goal_need'
      case 'awaiting_approval':
        return summary.action === null || summary.action === undefined ? null : 'decide_approval'
      case 'ambiguous':
        return 'reconcile_turn'
      case 'awaiting_reconciliation':
      case 'runner_lost':
      case 'awaiting_tool_recovery':
        return null
      case 'active':
      case 'queued':
      case 'parked':
      case 'idle':
        return null
    }
  })()
  if ((summary.action ?? null) !== expectedAction) {
    throw new TypeError('attention summary state and action are incoherent')
  }
  if (
    summary.state === 'blocked' &&
    (summary.goal_block === null || summary.goal_block === undefined)
  ) {
    throw new TypeError('blocked attention summary must include goal-block evidence')
  }
  if (summary.goal_block != null) {
    validateCursor(summary.goal_block.generation)
    if (summary.state !== 'blocked' && summary.state !== 'runner_lost') {
      throw new TypeError('attention summary state and goal-block evidence are incoherent')
    }
  }
}

const validateAttentionSnapshot = (
  snapshot: WebAttentionSnapshot,
  afterSessionId?: string,
): WebAttentionSnapshot => {
  validateCursor(snapshot.cursor)
  if (snapshot.summaries.length > MAX_ATTENTION_SNAPSHOT_ITEMS) {
    throw new TypeError('attention snapshot exceeds the contract item ceiling')
  }
  const sessionIds = new Set(snapshot.summaries.map((summary) => summary.session_id))
  if (sessionIds.size !== snapshot.summaries.length) {
    throw new TypeError('attention snapshot contains duplicate session identities')
  }
  for (const summary of snapshot.summaries) validateAttentionSummary(summary)
  if (
    afterSessionId !== undefined &&
    snapshot.summaries.some((summary) => summary.session_id <= afterSessionId)
  ) {
    throw new TypeError('attention snapshot contains an identity at or before its keyset cursor')
  }
  for (let index = 1; index < snapshot.summaries.length; index += 1) {
    const previous = snapshot.summaries[index - 1]
    const current = snapshot.summaries[index]
    if (!previous || !current) continue
    if (previous.session_id >= current.session_id) {
      throw new TypeError('attention snapshot summaries are not ordered by session identity')
    }
  }
  const lastSessionId = snapshot.summaries.at(-1)?.session_id ?? null
  const continuation = snapshot.continuation_after_session_id ?? null
  if (continuation !== null && continuation !== lastSessionId) {
    throw new TypeError('attention snapshot continuation does not match its last session identity')
  }
  if (continuation !== null && snapshot.summaries.length !== MAX_ATTENTION_SNAPSHOT_ITEMS) {
    throw new TypeError('continued attention snapshot must contain a full contract page')
  }
  return snapshot
}

const catalogTurnDerivedStates = new Set<WebSessionCatalogSnapshot['summaries'][number]['state']>([
  'active',
  'queued',
  'awaiting_approval',
  'ambiguous',
  'awaiting_tool_recovery',
  'awaiting_reconciliation',
])

const validateSessionCatalogSnapshot = (
  snapshot: WebSessionCatalogSnapshot,
  request: ProductSessionRequest,
): WebSessionCatalogSnapshot => {
  validateCursor(snapshot.cursor)
  validateCursor(snapshot.total)
  if (snapshot.summaries.length > 0 && snapshot.cursor === '0') {
    throw new TypeError('nonempty session catalog snapshot carries the empty cursor')
  }
  if (snapshot.summaries.length > MAX_SESSION_PAGE_ITEMS) {
    throw new TypeError('session catalog snapshot exceeds the contract item ceiling')
  }
  if (BigInt(snapshot.total) < BigInt(snapshot.summaries.length)) {
    throw new TypeError('session catalog total is smaller than its returned page')
  }
  const expectedSort =
    request.sort === 'identity' ? 'session_identity_ascending' : 'last_activity_descending'
  if (snapshot.sort !== expectedSort) {
    throw new TypeError('session catalog sort contradicts the request')
  }
  const identities = new Set<string>()
  for (const summary of snapshot.summaries) {
    if (!CATALOG_SESSION_ID_PATTERN.test(summary.session_id)) {
      throw new TypeError('session catalog contains a non-canonical session identity')
    }
    if (
      summary.current_turn_id !== null &&
      !CATALOG_SESSION_ID_PATTERN.test(summary.current_turn_id)
    ) {
      throw new TypeError('session catalog contains a non-canonical current-turn identity')
    }
    if (catalogTurnDerivedStates.has(summary.state) && summary.current_turn_id === null) {
      throw new TypeError('turn-derived session catalog state lacks a current-turn identity')
    }
    if (identities.has(summary.session_id)) {
      throw new TypeError('session catalog contains duplicate session identities')
    }
    identities.add(summary.session_id)
    if (!request.includeArchived && summary.archived) {
      throw new TypeError('session catalog contains an excluded archived session')
    }
    if (
      request.search !== undefined &&
      !summary.title_truncated &&
      !summary.session_id.includes(request.search) &&
      !(summary.title_summary?.includes(request.search) ?? false)
    ) {
      throw new TypeError('session catalog row contradicts the active search')
    }
    validateCursor(summary.active_turn_count)
    validateCursor(summary.queued_turn_count)
    validateCursor(summary.judge.actionable)
    validateCursor(summary.judge.completed)
    validateCursor(summary.judge.escalated)
    validateCursor(summary.judge.failed)
    validateCursor(summary.last_activity.unix_microseconds)
    if (summary.goal_block !== null && summary.goal_block !== undefined) {
      validatePositiveU64(summary.goal_block.generation, 'session catalog goal generation')
    }
    const titleScalars =
      summary.title_summary === null ? 0 : Array.from(summary.title_summary).length
    if (
      titleScalars > MAX_SESSION_SUMMARY_SCALARS ||
      (summary.title_truncated && titleScalars !== MAX_SESSION_SUMMARY_SCALARS) ||
      (summary.goal_block !== null &&
        summary.goal_block !== undefined &&
        Array.from(summary.goal_block.need_summary).length > MAX_SESSION_SUMMARY_SCALARS)
    ) {
      throw new TypeError('session catalog summary exceeds its scalar ceiling')
    }
    const milliseconds = Number(BigInt(summary.last_activity.unix_microseconds) / 1_000n)
    if (!Number.isSafeInteger(milliseconds) || !Number.isFinite(new Date(milliseconds).getTime())) {
      throw new TypeError('session catalog activity timestamp is outside the Date range')
    }
  }
  const first = snapshot.summaries[0]
  if (
    request.sort === 'identity' &&
    request.afterSession !== undefined &&
    first !== undefined &&
    first.session_id <= request.afterSession
  ) {
    throw new TypeError('session catalog response precedes its identity continuation')
  }
  if (
    request.sort === 'activity' &&
    request.afterActivity !== undefined &&
    first !== undefined &&
    BigInt(first.last_activity.unix_microseconds) > BigInt(request.afterActivity)
  ) {
    throw new TypeError('session catalog response precedes its activity continuation')
  }
  if (
    request.sort === 'activity' &&
    request.afterActivity !== undefined &&
    request.afterSession !== undefined &&
    first !== undefined &&
    first.last_activity.unix_microseconds === request.afterActivity &&
    first.session_id <= request.afterSession
  ) {
    throw new TypeError('session catalog response repeats its activity continuation boundary')
  }
  if (
    request.afterSession === undefined &&
    snapshot.continuation === null &&
    BigInt(snapshot.total) > BigInt(snapshot.summaries.length)
  ) {
    throw new TypeError('session catalog response omits a required continuation')
  }
  const continuation = snapshot.continuation
  if (continuation !== null) {
    const expectedKind = request.sort === 'identity' ? 'session_identity' : 'last_activity'
    if (continuation.kind !== expectedKind) {
      throw new TypeError('session catalog continuation contradicts the requested sort')
    }
    if (snapshot.summaries.length !== MAX_SESSION_PAGE_ITEMS) {
      throw new TypeError('continued session catalog snapshot is not a full page')
    }
    if (BigInt(snapshot.total) <= BigInt(snapshot.summaries.length)) {
      throw new TypeError('session catalog continuation contradicts the declared total')
    }
  }
  return snapshot
}

const decodeBoundedAttentionSnapshot = (
  value: unknown,
  afterSessionId?: string,
): WebAttentionSnapshot =>
  validateAttentionSnapshot(decodeWebAttentionSnapshot(value), afterSessionId)

const decodeAttentionLines = async function* (
  body: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<WebAttentionStreamEvent> {
  const reader = body.getReader()
  const decoder = new TextDecoder('utf-8', { fatal: true })
  let line: number[] = []
  let completed = false
  try {
    while (true) {
      let chunk: ReadableStreamReadResult<Uint8Array>
      try {
        chunk = await reader.read()
      } catch (error) {
        if (signal?.aborted) throw error
        throw new ProductTransportError(error)
      }
      if (chunk.done) {
        completed = true
        break
      }
      for (const byte of chunk.value) {
        if (byte === 10) {
          if (line.length === 0) throw new TypeError('attention stream contains an empty item')
          const value = JSON.parse(decoder.decode(Uint8Array.from(line)))
          line = []
          const event = decodeWebAttentionStreamEvent(value)
          if (event.kind === 'snapshot') validateAttentionSnapshot(event.snapshot)
          else {
            validateCursor(event.cursor)
            if (event.kind === 'update') {
              if (event.summaries.length > MAX_ATTENTION_SNAPSHOT_ITEMS) {
                throw new TypeError('attention update exceeds the contract item ceiling')
              }
              for (const summary of event.summaries) validateAttentionSummary(summary)
            }
          }
          yield event
        } else {
          if (line.length === MAX_NDJSON_ITEM_BYTES) {
            throw new TypeError('attention stream item exceeds the contract ceiling')
          }
          line.push(byte)
        }
      }
    }
    if (line.length !== 0) throw new TypeError('attention stream ended with an incomplete item')
  } finally {
    if (!completed) await reader.cancel().catch(() => undefined)
    reader.releaseLock()
  }
}

const utf8Length = (value: string): number => new TextEncoder().encode(value).byteLength

const validateBlobDescriptorInput = (input: BlobDescriptorInput): void => {
  if (utf8Length(input.mediaType) > MAX_DECLARED_MEDIA_TYPE_BYTES) {
    throw new ProductInputError('Descriptor media type exceeded the 255-byte limit.')
  }
  if (
    input.displayFilename !== undefined &&
    utf8Length(input.displayFilename) > MAX_DISPLAY_FILENAME_BYTES
  ) {
    throw new ProductInputError('Descriptor display filename exceeded the 1024-byte limit.')
  }
}

const readBoundedJson = async (
  response: Response,
  maximumBytes: number = MAX_PRODUCT_JSON_BYTES,
): Promise<unknown> => {
  const declaredLength = Number(response.headers.get('content-length'))
  if (Number.isFinite(declaredLength) && declaredLength > maximumBytes) {
    throw new Error('response exceeded the product JSON byte limit')
  }

  if (!response.body) {
    const text = await response.text()
    if (new TextEncoder().encode(text).byteLength > maximumBytes) {
      throw new Error('response exceeded the product JSON byte limit')
    }
    return JSON.parse(text)
  }

  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let received = 0
  while (true) {
    let result: ReadableStreamReadResult<Uint8Array>
    try {
      result = await reader.read()
    } catch (error) {
      throw new ProductTransportError(error)
    }
    if (result.done) break
    received += result.value.byteLength
    if (received > maximumBytes) {
      await reader.cancel()
      throw new Error('response exceeded the product JSON byte limit')
    }
    chunks.push(result.value)
  }

  const bytes = new Uint8Array(received)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
}

const request = async (input: RequestInfo | URL, init: RequestInit): Promise<Response> => {
  try {
    return await fetch(input, init)
  } catch (error) {
    throw new ProductTransportError(error)
  }
}

const canonicalizedSearchUuid = (value: string): string | undefined => {
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

const validateSearchPageBounds = (
  page: WebSearchPage,
  searchRequest: ProductSearchRequest,
): WebSearchPage => {
  if (page.results.length > searchRequest.maxItems)
    throw new TypeError('search page exceeds item limit')
  const encoder = new TextEncoder()
  const requestedSession =
    searchRequest.sessionId === undefined
      ? undefined
      : canonicalizedSearchUuid(searchRequest.sessionId)
  let previousAddress =
    searchRequest.after === undefined ? undefined : BigInt(searchRequest.after.address)
  let previousProjectionId =
    searchRequest.after === undefined ? undefined : BigInt(searchRequest.after.projectionId)
  let firstResult = true
  for (const result of page.results) {
    const resultSession = canonicalizedSearchUuid(result.session_id)
    if (
      resultSession === undefined ||
      sourceUuids(result.source).some((identity) => canonicalizedSearchUuid(identity) === undefined)
    ) {
      throw new TypeError('search result carries an invalid UUID identity')
    }
    if (
      searchRequest.sessionId !== undefined &&
      (requestedSession === undefined || resultSession !== requestedSession)
    ) {
      throw new TypeError('search result falls outside the requested session')
    }
    if (
      result.source.kind === 'session' &&
      canonicalizedSearchUuid(result.source.session_id) !== resultSession
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
        firstResult && searchRequest.after !== undefined
          ? 'search page does not advance past the request cursor'
          : 'search page is not ordered newest first',
      )
    }
    firstResult = false
    previousAddress = address
    previousProjectionId = projectionId
    const snippetBytes = encoder.encode(result.snippet)
    const snippetLength = snippetBytes.byteLength
    if (snippetLength > searchRequest.maxSnippetBytes) {
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

export const readProductRouteState = (value: Record<string, unknown>): ProductRouteState => {
  const catalog = readProductSessionState(value)
  return {
    ...catalog,
    ...readProductSearchState(value),
  }
}

const validateBootstrapSearchLimits = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
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

const validateCurrentBootstrap = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
  if (
    bootstrap.contract.name !== 'signalbox.web-http' ||
    bootstrap.contract.version !== '2' ||
    bootstrap.limits.max_json_body_bytes !== MAX_PRODUCT_JSON_BYTES ||
    bootstrap.limits.max_ndjson_item_bytes !== MAX_NDJSON_ITEM_BYTES ||
    !bootstrap.capabilities.bounded_json ||
    !bootstrap.capabilities.same_origin_json_mutations ||
    !bootstrap.capabilities.ndjson_streaming ||
    (bootstrap.capabilities.blob_derivations && !bootstrap.capabilities.immutable_blob_content) ||
    (bootstrap.capabilities.image_derivatives && !bootstrap.capabilities.blob_derivations)
  ) {
    throw new Error('bootstrap contradicted the fixed signalbox.web-http v2 contract')
  }
  return bootstrap
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await request('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    try {
      return validateBootstrapSearchLimits(
        validateCurrentBootstrap(decodeWebContractBootstrap(await readBoundedJson(response))),
      )
    } catch (error) {
      if (error instanceof ProductTransportError) throw error
      throw new ProductContractError(error)
    }
  }

  async readSessions(
    catalogRequest: ProductSessionRequest,
    signal?: AbortSignal,
  ): Promise<WebSessionCatalogSnapshot> {
    if (
      catalogRequest.search !== undefined &&
      admittedSessionSearch(catalogRequest.search) === undefined
    ) {
      throw new TypeError('session catalog search exceeds its contract bound')
    }
    const query = new URLSearchParams({
      include_archived: String(catalogRequest.includeArchived),
      sort:
        catalogRequest.sort === 'identity'
          ? 'session_identity_ascending'
          : 'last_activity_descending',
    })
    if (catalogRequest.search !== undefined) query.set('search', catalogRequest.search)
    if (catalogRequest.afterSession !== undefined) {
      query.set('after_session_id', catalogRequest.afterSession)
    }
    if (catalogRequest.afterActivity !== undefined) {
      query.set('after_activity_unix_microseconds', catalogRequest.afterActivity)
    }
    const response = await request(`/api/sessions?${query}`, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    const payload = await readBoundedJson(response)
    if (!response.ok) {
      throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
    }
    return validateSessionCatalogSnapshot(decodeWebSessionCatalogSnapshot(payload), catalogRequest)
  }

  async readBlobDescriptor(
    input: BlobDescriptorInput,
    signal?: AbortSignal,
  ): Promise<WebBlobDescriptor> {
    validateBlobDescriptorInput(input)
    const query = new URLSearchParams({ media_type: input.mediaType })
    if (input.displayFilename) query.set('display_filename', input.displayFilename)
    const response = await request(
      `/api/blobs/${encodeURIComponent(input.digest)}/descriptor?${query.toString()}`,
      {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
        signal,
      },
    )
    const payload = await readBoundedJson(response)
    if (!response.ok) {
      throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
    }
    const descriptor = decodeWebBlobDescriptor(payload)
    if (descriptor.digest !== input.digest) {
      throw new Error('descriptor digest did not match the requested blob identity')
    }
    if (descriptor.declared_media_type !== input.mediaType) {
      throw new Error('descriptor media type did not match the requested blob use')
    }
    const expectedFilenames = input.displayFilename ? [input.displayFilename] : []
    if (
      descriptor.display_filename.length !== expectedFilenames.length ||
      descriptor.display_filename.some((filename, index) => filename !== expectedFilenames[index])
    ) {
      throw new Error('descriptor filename did not match the requested blob use')
    }
    return descriptor
  }

  async readAttention(
    afterSessionId?: string,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    const query = new URLSearchParams()
    if (afterSessionId) query.set('after_session_id', afterSessionId)
    const path = query.size === 0 ? '/api/attention' : `/api/attention?${query}`
    const response = await request(path, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    const payload = await readBoundedJson(response)
    if (!response.ok) {
      throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
    }
    return decodeBoundedAttentionSnapshot(payload, afterSessionId)
  }

  async *followAttention(signal?: AbortSignal): AsyncGenerator<WebAttentionStreamEvent> {
    const response = await request('/api/attention/follow', {
      headers: { accept: 'application/x-ndjson' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) {
      throw new ProductRequestError(
        response.status,
        decodeWebApiErrorResponse(await readBoundedJson(response)),
      )
    }
    const mediaType = response.headers.get('content-type')?.split(';', 1)[0]?.trim().toLowerCase()
    if (mediaType !== 'application/x-ndjson') {
      await response.body?.cancel().catch(() => undefined)
      throw new TypeError('attention stream response must use application/x-ndjson')
    }
    if (!response.body) throw new TypeError('attention stream response has no body')
    yield* decodeAttentionLines(response.body, signal)
  }

  async search(searchRequest: ProductSearchRequest, signal?: AbortSignal): Promise<WebSearchPage> {
    const query = new URLSearchParams({
      strategy: 'lexical',
      q: searchRequest.query,
      max_items: String(searchRequest.maxItems),
    })
    if (searchRequest.sessionId) query.set('session_id', searchRequest.sessionId)
    if (searchRequest.after) {
      query.set('after_address', searchRequest.after.address)
      query.set('after_projection', searchRequest.after.projectionId)
    }
    const response = await request(`/api/search?${query}`, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) {
      throw new ProductRequestError(
        response.status,
        decodeWebApiErrorResponse(await readBoundedJson(response)),
      )
    }
    const page = decodeWebSearchPage(await readBoundedJson(response, MAX_SEARCH_RESPONSE_BYTES))
    return validateSearchPageBounds(page, searchRequest)
  }
}

export const productTransport = new SameOriginProductTransport()

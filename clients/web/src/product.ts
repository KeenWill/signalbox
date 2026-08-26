import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebBlobDescriptor,
  decodeWebContractBootstrap,
  type WebApiErrorResponse,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebBlobDescriptor,
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
  activity: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['system activity reads'],
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
  readBlobDescriptor(input: BlobDescriptorInput, signal?: AbortSignal): Promise<WebBlobDescriptor>
  readAttention(afterSessionId?: string, signal?: AbortSignal): Promise<WebAttentionSnapshot>
  followAttention(signal?: AbortSignal): AsyncIterable<WebAttentionStreamEvent>
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

const MAX_UNSIGNED_64 = 18_446_744_073_709_551_615n
const SESSION_ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const CANONICAL_NONNEGATIVE_INTEGER_PATTERN = /^(0|[1-9]\d*)$/

const validateCursor = (cursor: string): void => {
  if (!CANONICAL_NONNEGATIVE_INTEGER_PATTERN.test(cursor) || BigInt(cursor) > MAX_UNSIGNED_64) {
    throw new TypeError('attention cursor must be a canonical unsigned 64-bit integer')
  }
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

const readBoundedJson = async (response: Response): Promise<unknown> => {
  const declaredLength = Number(response.headers.get('content-length'))
  if (Number.isFinite(declaredLength) && declaredLength > MAX_PRODUCT_JSON_BYTES) {
    throw new Error('response exceeded the product JSON byte limit')
  }

  if (!response.body) return JSON.parse(await response.text())

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
    if (received > MAX_PRODUCT_JSON_BYTES) {
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
      return validateCurrentBootstrap(decodeWebContractBootstrap(await readBoundedJson(response)))
    } catch (error) {
      if (error instanceof ProductTransportError) throw error
      throw new ProductContractError(error)
    }
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
}

export const productTransport = new SameOriginProductTransport()

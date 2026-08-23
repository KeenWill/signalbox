import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebContractBootstrap,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebContractBootstrap,
} from './generated/web-contract.mjs'

// The version-one browser contract fixes the NDJSON item ceiling at 65,536 bytes.
const MAX_ATTENTION_EVENT_BYTES = 65_536
export const MAX_BOOTSTRAP_BYTES = 65_536
export const MAX_ATTENTION_SNAPSHOT_BYTES = 65_536
export const MAX_ATTENTION_SNAPSHOT_ITEMS = 32
const MAX_UNSIGNED_64 = 18_446_744_073_709_551_615n
const SESSION_ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

const validateCursor = (cursor: string): void => {
  if (!/^(0|[1-9]\d*)$/.test(cursor) || BigInt(cursor) > MAX_UNSIGNED_64) {
    throw new TypeError('attention cursor must be a canonical unsigned 64-bit integer')
  }
}

const validateBootstrap = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
  if (
    bootstrap.contract.name !== 'signalbox.web-http' ||
    bootstrap.contract.version !== '1' ||
    !bootstrap.capabilities.bounded_json ||
    !bootstrap.capabilities.same_origin_json_mutations ||
    !bootstrap.capabilities.ndjson_streaming ||
    bootstrap.limits.max_json_body_bytes !== MAX_BOOTSTRAP_BYTES ||
    bootstrap.limits.max_ndjson_item_bytes !== MAX_ATTENTION_EVENT_BYTES
  ) {
    throw new TypeError('bootstrap carries an incompatible web contract')
  }
  return bootstrap
}

type AttentionSummary = WebAttentionSnapshot['summaries'][number]

const validateAttentionSummary = (summary: AttentionSummary): void => {
  if (!SESSION_ID_PATTERN.test(summary.session_id)) {
    throw new TypeError('attention summary session identity must be a canonical UUID')
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
        return 'provide_goal_need'
      case 'awaiting_approval':
        return summary.action === null || summary.action === undefined ? null : 'decide_approval'
      case 'ambiguous':
      case 'awaiting_reconciliation':
        return 'reconcile_turn'
      case 'runner_lost':
        return 'restore_runner'
      case 'active':
      case 'queued':
      case 'idle':
        return null
    }
  })()
  if ((summary.action ?? null) !== expectedAction) {
    throw new TypeError('attention summary state and action are incoherent')
  }
}

const validateAttentionSnapshot = (snapshot: WebAttentionSnapshot): WebAttentionSnapshot => {
  validateCursor(snapshot.cursor)
  if (snapshot.summaries.length > MAX_ATTENTION_SNAPSHOT_ITEMS) {
    throw new TypeError('attention snapshot exceeds the contract item ceiling')
  }
  const sessionIds = new Set(snapshot.summaries.map((summary) => summary.session_id))
  if (sessionIds.size !== snapshot.summaries.length) {
    throw new TypeError('attention snapshot contains duplicate session identities')
  }
  for (const summary of snapshot.summaries) validateAttentionSummary(summary)
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
  return snapshot
}

const decodeBoundedAttentionSnapshot = (value: unknown): WebAttentionSnapshot =>
  validateAttentionSnapshot(decodeWebAttentionSnapshot(value))

const readBoundedJson = async (
  response: Response,
  maximumBytes: number,
  resource: string,
  signal?: AbortSignal,
): Promise<unknown> => {
  if (!response.body) throw new TypeError(`${resource} response has no body`)
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let byteLength = 0
  try {
    while (true) {
      let chunk: ReadableStreamReadResult<Uint8Array>
      try {
        chunk = await reader.read()
      } catch (error) {
        if (signal?.aborted) throw error
        throw new ProductRequestError(
          'transport_unavailable',
          'transport',
          `Network request failed while reading the ${resource}.`,
        )
      }
      if (chunk.done) break
      byteLength += chunk.value.byteLength
      if (byteLength > maximumBytes) {
        await reader.cancel().catch(() => undefined)
        throw new TypeError(`${resource} exceeds the contract byte ceiling`)
      }
      chunks.push(chunk.value)
    }
  } finally {
    reader.releaseLock()
  }
  const encoded = new Uint8Array(byteLength)
  let offset = 0
  for (const chunk of chunks) {
    encoded.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(encoded))
}

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
        throw new ProductRequestError(
          'transport_unavailable',
          'transport',
          'Network request failed while reading the attention stream.',
        )
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
          if (line.length === MAX_ATTENTION_EVENT_BYTES) {
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

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
  readAttention(afterSessionId?: string, signal?: AbortSignal): Promise<WebAttentionSnapshot>
  followAttention(signal?: AbortSignal): AsyncIterable<WebAttentionStreamEvent>
}

export class ProductRequestError extends Error {
  constructor(
    readonly code: string,
    readonly kind: 'transport' | 'application',
    message: string,
  ) {
    super(message)
    this.name = 'ProductRequestError'
  }
}

const fetchProductResource = async (input: string, init: RequestInit): Promise<Response> => {
  try {
    return await fetch(input, init)
  } catch (error) {
    if (init.signal?.aborted) throw error
    throw new ProductRequestError(
      'transport_unavailable',
      'transport',
      'Network request failed before a response was received.',
    )
  }
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await fetchProductResource('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) {
      await response.body?.cancel().catch(() => undefined)
      throw new ProductRequestError(
        'transport_unavailable',
        'transport',
        `Bootstrap request failed with status ${response.status}.`,
      )
    }
    return validateBootstrap(
      decodeWebContractBootstrap(
        await readBoundedJson(response, MAX_BOOTSTRAP_BYTES, 'bootstrap response', signal),
      ),
    )
  }

  async readAttention(
    afterSessionId?: string,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    const query = new URLSearchParams()
    if (afterSessionId) query.set('after_session_id', afterSessionId)
    const path = query.size === 0 ? '/api/attention' : `/api/attention?${query}`
    const response = await fetchProductResource(path, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response, signal)
    return decodeBoundedAttentionSnapshot(
      await readBoundedJson(response, MAX_ATTENTION_SNAPSHOT_BYTES, 'attention snapshot', signal),
    )
  }

  async *followAttention(signal?: AbortSignal): AsyncGenerator<WebAttentionStreamEvent> {
    const response = await fetchProductResource('/api/attention/follow', {
      headers: { accept: 'application/x-ndjson' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response, signal)
    if (!response.body) throw new TypeError('attention stream response has no body')
    yield* decodeAttentionLines(response.body, signal)
  }

  private async requestError(
    response: Response,
    signal?: AbortSignal,
  ): Promise<ProductRequestError> {
    const failure = decodeWebApiErrorResponse(
      await readBoundedJson(response, MAX_ATTENTION_SNAPSHOT_BYTES, 'error response', signal),
    )
    return new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
  }
}

export const productTransport = new SameOriginProductTransport()

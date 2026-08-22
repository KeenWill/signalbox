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

const decodeAttentionLines = async function* (
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<WebAttentionStreamEvent> {
  const reader = body.getReader()
  const decoder = new TextDecoder('utf-8', { fatal: true })
  let line: number[] = []
  try {
    while (true) {
      const chunk = await reader.read()
      if (chunk.done) break
      for (const byte of chunk.value) {
        if (byte === 10) {
          if (line.length === 0) throw new TypeError('attention stream contains an empty item')
          const value = JSON.parse(decoder.decode(Uint8Array.from(line)))
          line = []
          yield decodeWebAttentionStreamEvent(value)
        } else {
          if (line.length === MAX_ATTENTION_EVENT_BYTES) {
            throw new TypeError('attention stream item exceeds the contract ceiling')
          }
          line.push(byte)
        }
      }
    }
    if (line.length !== 0) throw new TypeError('attention stream ended with an incomplete item')
  } catch (error) {
    await reader.cancel().catch(() => undefined)
    throw error
  } finally {
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

  async readAttention(
    afterSessionId?: string,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    const query = new URLSearchParams()
    if (afterSessionId) query.set('after_session_id', afterSessionId)
    const path = query.size === 0 ? '/api/attention' : `/api/attention?${query}`
    const response = await fetch(path, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response)
    return decodeWebAttentionSnapshot(await response.json())
  }

  async *followAttention(signal?: AbortSignal): AsyncGenerator<WebAttentionStreamEvent> {
    const response = await fetch('/api/attention/follow', {
      headers: { accept: 'application/x-ndjson' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response)
    if (!response.body) throw new TypeError('attention stream response has no body')
    yield* decodeAttentionLines(response.body)
  }

  private async requestError(response: Response): Promise<ProductRequestError> {
    const failure = decodeWebApiErrorResponse(await response.json())
    return new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
  }
}

export const productTransport = new SameOriginProductTransport()

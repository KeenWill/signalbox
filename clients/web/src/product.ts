import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebContractBootstrap,
  decodeWebRepoWatchActivityPage,
  decodeWebRepoWatchPullRequestPage,
  decodeWebRepoWatchPullRequestSessionPage,
  decodeWebRepoWatchRepositoryStatusPage,
  decodeWebRepoWatchWorkPage,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebContractBootstrap,
  type WebRepoWatchActivityPage,
  type WebRepoWatchPullRequestPage,
  type WebRepoWatchPullRequestSessionPage,
  type WebRepoWatchRepositoryStatusPage,
  type WebRepoWatchWorkPage,
} from './generated/web-contract.mjs'

// The version-one browser contract fixes both transport ceilings at 65,536 bytes.
const MAX_JSON_BODY_BYTES = 65_536
const MAX_ATTENTION_EVENT_BYTES = 65_536
const MAX_POSTGRES_BIGINT = 9_223_372_036_854_775_807n
const MAX_POSTGRES_INTEGER = 2_147_483_647
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

const canonicalUuid = (value: string, field: string): string => {
  if (!CANONICAL_UUID.test(value)) throw new TypeError(`${field} is not canonical`)
  return value
}

const validateAttentionPage = (
  afterSessionId: string | undefined,
  page: WebAttentionSnapshot,
): WebAttentionSnapshot => {
  if (!afterSessionId) return page
  const requested = canonicalUuid(afterSessionId, 'requested attention cursor')
  let previous = requested
  for (const summary of page.summaries) {
    const session = canonicalUuid(summary.session_id, 'attention page session')
    if (session <= previous) {
      throw new TypeError('attention page does not advance beyond the requested cursor')
    }
    previous = session
  }
  const continuationCursor = page.continuation_after_session_id
  if (continuationCursor != null) {
    const continuation = canonicalUuid(continuationCursor, 'attention continuation')
    if (continuation <= requested || continuation < previous) {
      throw new TypeError('attention continuation does not advance beyond the requested cursor')
    }
  }
  return page
}

const validateRepositoryPage = (
  afterRepository: string | undefined,
  page: WebRepoWatchRepositoryStatusPage,
): WebRepoWatchRepositoryStatusPage => {
  if (!afterRepository) return page
  let previous = afterRepository
  for (const status of page.repositories) {
    if (status.repository <= previous) {
      throw new TypeError('repository page does not advance beyond the requested cursor')
    }
    previous = status.repository
  }
  const continuation = page.continuation_after_repository
  if (continuation != null && (continuation <= afterRepository || continuation < previous)) {
    throw new TypeError('repository continuation does not advance beyond the requested cursor')
  }
  return page
}

const canonicalPositiveBigInt = (value: string, field: string): bigint => {
  if (!/^[1-9][0-9]*$/.test(value)) throw new TypeError(`${field} is not canonical`)
  const parsed = BigInt(value)
  if (parsed > MAX_POSTGRES_BIGINT) throw new TypeError(`${field} exceeds its database range`)
  return parsed
}

const validateActivityContinuations = (
  window: RepoWatchActivityWindow | undefined,
  page: WebRepoWatchActivityPage,
): WebRepoWatchActivityPage => {
  const event = page.event_continuation_before
  if (event) {
    const generation = canonicalPositiveBigInt(
      event.cursor_generation,
      'event continuation generation',
    )
    if (event.event_ordinal < 1 || event.event_ordinal > MAX_POSTGRES_INTEGER) {
      throw new TypeError('event continuation ordinal exceeds its database range')
    }
    const requested = window?.eventBefore
    if (requested) {
      const requestedGeneration = canonicalPositiveBigInt(
        requested.cursorGeneration,
        'requested event generation',
      )
      if (
        generation > requestedGeneration ||
        (generation === requestedGeneration && event.event_ordinal >= requested.eventOrdinal)
      ) {
        throw new TypeError('event continuation does not advance to older history')
      }
    }
  }

  const webhook = page.webhook_continuation_before_receipt_sequence
  if (webhook) {
    const sequence = canonicalPositiveBigInt(webhook, 'webhook continuation')
    const requested = window?.webhookBeforeReceiptSequence
    if (requested && sequence >= canonicalPositiveBigInt(requested, 'requested webhook cursor')) {
      throw new TypeError('webhook continuation does not advance to older history')
    }
  }
  return page
}

const requireCompatibleBootstrap = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
  if (
    !bootstrap.capabilities.bounded_json ||
    !bootstrap.capabilities.same_origin_json_mutations ||
    !bootstrap.capabilities.ndjson_streaming ||
    bootstrap.limits.max_json_body_bytes !== MAX_JSON_BODY_BYTES ||
    bootstrap.limits.max_ndjson_item_bytes !== MAX_ATTENTION_EVENT_BYTES
  ) {
    throw new TypeError('bootstrap capabilities or limits are incompatible with this client')
  }
  return bootstrap
}

const readBoundedJson = async (response: Response): Promise<unknown> => {
  if (!response.body) throw new TypeError('JSON response has no body')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let length = 0
  let complete = false
  try {
    while (true) {
      const chunk = await reader.read()
      if (chunk.done) {
        complete = true
        break
      }
      if (length + chunk.value.byteLength > MAX_JSON_BODY_BYTES) {
        throw new TypeError('JSON response exceeds the contract ceiling')
      }
      chunks.push(chunk.value)
      length += chunk.value.byteLength
    }
    const encoded = new Uint8Array(length)
    let offset = 0
    for (const chunk of chunks) {
      encoded.set(chunk, offset)
      offset += chunk.byteLength
    }
    return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(encoded))
  } finally {
    if (!complete) await reader.cancel().catch(() => undefined)
    reader.releaseLock()
  }
}

const decodeAttentionLines = async function* (
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<WebAttentionStreamEvent> {
  const reader = body.getReader()
  const decoder = new TextDecoder('utf-8', { fatal: true })
  let line: number[] = []
  let complete = false
  try {
    while (true) {
      const chunk = await reader.read()
      if (chunk.done) {
        complete = true
        break
      }
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
  } finally {
    if (!complete) await reader.cancel().catch(() => undefined)
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

export interface RepoWatchHeldCursor {
  heldSinceUnixMilliseconds: string
  dispatchId: string
}

export interface RepoWatchObligationCursor {
  owedSinceUnixMilliseconds: string
  obligationId: string
}

export interface RepoWatchSessionCursor {
  commissionedAtUnixMilliseconds: string
  sessionId: string
}

export interface RepoWatchEventCursor {
  cursorGeneration: string
  eventOrdinal: number
}

export interface RepoWatchActivityWindow {
  eventBefore?: RepoWatchEventCursor
  webhookBeforeReceiptSequence?: string
  includeEvents: boolean
  includeWebhooks: boolean
}

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
  readAttention(afterSessionId?: string, signal?: AbortSignal): Promise<WebAttentionSnapshot>
  followAttention(signal?: AbortSignal): AsyncIterable<WebAttentionStreamEvent>
}

export interface RepoWatchProductTransport {
  readRepoWatchRepositories(
    afterRepository?: string,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchRepositoryStatusPage>
  readRepoWatchPullRequests(
    repository: string,
    afterPullRequest?: string,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchPullRequestPage>
  readRepoWatchWork(
    repository: string,
    heldAfter?: RepoWatchHeldCursor,
    obligationAfter?: RepoWatchObligationCursor,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchWorkPage>
  readRepoWatchPullRequestSessions(
    repository: string,
    pullRequest: string,
    before?: RepoWatchSessionCursor,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchPullRequestSessionPage>
  readRepoWatchActivity(
    repository: string,
    window?: RepoWatchActivityWindow,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchActivityPage>
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

export class SameOriginProductTransport implements ProductTransport, RepoWatchProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await this.fetchResponse('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    return requireCompatibleBootstrap(decodeWebContractBootstrap(await readBoundedJson(response)))
  }

  async readAttention(
    afterSessionId?: string,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    const query = new URLSearchParams()
    if (afterSessionId) query.set('after_session_id', afterSessionId)
    const path = query.size === 0 ? '/api/attention' : `/api/attention?${query}`
    const response = await this.fetchResponse(path, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response)
    return validateAttentionPage(
      afterSessionId,
      decodeWebAttentionSnapshot(await readBoundedJson(response)),
    )
  }

  async *followAttention(signal?: AbortSignal): AsyncGenerator<WebAttentionStreamEvent> {
    const response = await this.fetchResponse('/api/attention/follow', {
      headers: { accept: 'application/x-ndjson' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response)
    if (!response.body) throw new TypeError('attention stream response has no body')
    yield* decodeAttentionLines(response.body)
  }

  async readRepoWatchRepositories(
    afterRepository?: string,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchRepositoryStatusPage> {
    const query = new URLSearchParams()
    if (afterRepository) query.set('after_repository', afterRepository)
    const page = await this.readJson(
      this.queryPath('/api/repository-watch/repositories', query),
      decodeWebRepoWatchRepositoryStatusPage,
      signal,
    )
    return validateRepositoryPage(afterRepository, page)
  }

  async readRepoWatchPullRequests(
    repository: string,
    afterPullRequest?: string,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchPullRequestPage> {
    const query = new URLSearchParams({ repository })
    if (afterPullRequest) query.set('after_pull_request', afterPullRequest)
    const page = await this.readJson(
      this.queryPath('/api/repository-watch/pull-requests', query),
      decodeWebRepoWatchPullRequestPage,
      signal,
    )
    if (page.repository !== repository) {
      throw new TypeError('pull-request page repository does not match the requested repository')
    }
    if (afterPullRequest) {
      const requested = canonicalPositiveBigInt(afterPullRequest, 'requested pull-request cursor')
      let previous = requested
      for (const pullRequest of page.pull_requests) {
        const number = canonicalPositiveBigInt(pullRequest.number, 'pull-request page number')
        if (number <= previous) {
          throw new TypeError('pull-request page does not advance beyond the requested cursor')
        }
        previous = number
      }
      const continuationCursor = page.continuation_after_pull_request
      if (continuationCursor != null) {
        const continuation = canonicalPositiveBigInt(
          continuationCursor,
          'pull-request continuation',
        )
        if (continuation <= requested || continuation < previous) {
          throw new TypeError(
            'pull-request continuation does not advance beyond the requested cursor',
          )
        }
      }
    }
    return page
  }

  async readRepoWatchWork(
    repository: string,
    heldAfter?: RepoWatchHeldCursor,
    obligationAfter?: RepoWatchObligationCursor,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchWorkPage> {
    const query = new URLSearchParams({ repository })
    if (heldAfter) {
      query.set('held_after_unix_milliseconds', heldAfter.heldSinceUnixMilliseconds)
      query.set('held_after_dispatch_id', heldAfter.dispatchId)
    }
    if (obligationAfter) {
      query.set('obligation_after_unix_milliseconds', obligationAfter.owedSinceUnixMilliseconds)
      query.set('obligation_after_id', obligationAfter.obligationId)
    }
    return this.readJson(
      this.queryPath('/api/repository-watch/work', query),
      decodeWebRepoWatchWorkPage,
      signal,
    )
  }

  async readRepoWatchPullRequestSessions(
    repository: string,
    pullRequest: string,
    before?: RepoWatchSessionCursor,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchPullRequestSessionPage> {
    const query = new URLSearchParams({ repository, pull_request: pullRequest })
    if (before) {
      query.set('before_unix_milliseconds', before.commissionedAtUnixMilliseconds)
      query.set('before_session_id', before.sessionId)
    }
    return this.readJson(
      this.queryPath('/api/repository-watch/sessions', query),
      decodeWebRepoWatchPullRequestSessionPage,
      signal,
    )
  }

  async readRepoWatchActivity(
    repository: string,
    window?: RepoWatchActivityWindow,
    signal?: AbortSignal,
  ): Promise<WebRepoWatchActivityPage> {
    const query = new URLSearchParams({ repository })
    if (window) {
      query.set('include_events', window.includeEvents.toString())
      query.set('include_webhooks', window.includeWebhooks.toString())
    }
    if (window?.eventBefore) {
      query.set('event_before_cursor_generation', window.eventBefore.cursorGeneration)
      query.set('event_before_ordinal', window.eventBefore.eventOrdinal.toString())
    }
    if (window?.webhookBeforeReceiptSequence) {
      query.set('webhook_before_receipt_sequence', window.webhookBeforeReceiptSequence)
    }
    const page = await this.readJson(
      this.queryPath('/api/repository-watch/activity', query),
      decodeWebRepoWatchActivityPage,
      signal,
    )
    return validateActivityContinuations(window, page)
  }

  private queryPath(path: string, query: URLSearchParams): string {
    return query.size === 0 ? path : `${path}?${query}`
  }

  private async readJson<T>(
    path: string,
    decode: (value: unknown) => T,
    signal?: AbortSignal,
  ): Promise<T> {
    const response = await this.fetchResponse(path, {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw await this.requestError(response)
    return decode(await readBoundedJson(response))
  }

  private async fetchResponse(input: RequestInfo | URL, init: RequestInit): Promise<Response> {
    try {
      return await fetch(input, init)
    } catch (error) {
      if (error instanceof Error && error.name === 'AbortError') throw error
      throw new ProductRequestError(
        'network_unavailable',
        'transport',
        'the daemon request could not be completed',
      )
    }
  }

  private async requestError(response: Response): Promise<ProductRequestError> {
    const failure = decodeWebApiErrorResponse(await readBoundedJson(response))
    return new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
  }
}

export const productTransport = new SameOriginProductTransport()

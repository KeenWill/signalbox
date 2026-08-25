import {
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebSessionLiveSnapshot,
  decodeWebSessionLiveStreamEvent,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebSessionLiveSnapshot,
  type WebSessionLiveStreamEvent,
} from '../generated/web-contract.mjs'

export const MAX_CATALOG_ROWS = 512
export const MAX_JSON_BODY_BYTES = 65_536
export const MAX_NDJSON_RECORD_BYTES = 65_536
export const MAX_PROVIDER_DRAFT_BYTES = 65_536
export const MAX_PROVIDER_DRAFT_PARTS = 32
export const MAX_LIVE_DURABLE_ITEMS = 128

export type CatalogSort = 'last_activity_desc' | 'session_id_asc'

export interface CatalogQuery {
  search: string
  sort: CatalogSort
}

export interface CatalogPresentation {
  snapshot: WebAttentionSnapshot | null
  summaries: WebAttentionSnapshot['summaries']
}

export interface ProviderDraft {
  key: string
  turnId: string
  modelCallId: string
  partIndex: number
  content: string
}

export interface LivePresentation {
  snapshot: WebSessionLiveSnapshot | null
  durable: ReadonlyArray<Extract<WebSessionLiveStreamEvent, { kind: 'durable' }>>
  drafts: ReadonlyArray<ProviderDraft>
  /**
   * The timeline address of the newest durable header evicted from the bounded
   * overlay, or null when no evicted header is missing from loaded history.
   * Global sequences are noncontiguous per session, so the gap is keyed by the
   * exact evicted address rather than derived from the oldest retained one.
   */
  durableGap: string | null
  resyncing: boolean
}

const historyCoversDurableGap = (
  durableGap: LivePresentation['durableGap'],
  historicalEventSequences?: ReadonlySet<string>,
): boolean => durableGap !== null && (historicalEventSequences?.has(durableGap) ?? false)

export type FollowConnectionState = 'connecting' | 'live' | 'retrying'

export const EMPTY_CATALOG_PRESENTATION: CatalogPresentation = {
  snapshot: null,
  summaries: [],
}

export const EMPTY_LIVE_PRESENTATION: LivePresentation = {
  snapshot: null,
  durable: [],
  drafts: [],
  durableGap: null,
  resyncing: false,
}

const continuationParams = (continuation: NonNullable<WebAttentionSnapshot['continuation']>) => {
  const params = new URLSearchParams({ after_session_id: continuation.session_id })
  if (continuation.kind === 'last_activity') {
    params.set('after_activity_unix_microseconds', continuation.unix_microseconds)
  }
  return params
}

export const catalogUrl = (
  query: CatalogQuery,
  continuation?: WebAttentionSnapshot['continuation'],
): string => {
  const params = continuation ? continuationParams(continuation) : new URLSearchParams()
  const search = query.search.trim()
  if (search) params.set('search', search)
  params.set('sort', query.sort)
  return `/api/sessions?${params.toString()}`
}

export const readBoundedJson = async (
  response: Response,
  maxBodyBytes = MAX_JSON_BODY_BYTES,
): Promise<unknown> => {
  if (!response.ok) throw new Error(`session read failed with status ${response.status}`)
  if (!response.body) throw new Error('session read response has no body')
  const reader = response.body.getReader()
  let body: Uint8Array<ArrayBufferLike> = new Uint8Array()
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      if (body.byteLength + value.byteLength > maxBodyBytes) {
        throw new Error('session JSON response exceeds the byte limit')
      }
      body = appendBytes(body, value)
    }
    return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(body))
  } finally {
    try {
      await reader.cancel()
    } catch {
      // The transport may already be closed while the bounded reader unwinds.
    }
    reader.releaseLock()
  }
}

export class HttpSessionProjectionSource {
  constructor(
    private readonly fetcher: typeof fetch = window.fetch.bind(window),
    private readonly maxJsonBodyBytes = MAX_JSON_BODY_BYTES,
    private readonly maxNdjsonRecordBytes = MAX_NDJSON_RECORD_BYTES,
  ) {}

  async catalogPage(
    query: CatalogQuery,
    continuation?: WebAttentionSnapshot['continuation'],
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    const response = await this.fetcher(catalogUrl(query, continuation), {
      credentials: 'same-origin',
      headers: { accept: 'application/json' },
      signal,
    })
    const snapshot = decodeWebAttentionSnapshot(
      await readBoundedJson(response, this.maxJsonBodyBytes),
    )
    const expectedSort =
      query.sort === 'last_activity_desc'
        ? 'last_activity_descending'
        : 'session_identity_ascending'
    const expectedContinuationKind =
      query.sort === 'last_activity_desc' ? 'last_activity' : 'session_identity'
    if (
      snapshot.sort !== expectedSort ||
      (snapshot.continuation !== null &&
        snapshot.continuation !== undefined &&
        snapshot.continuation.kind !== expectedContinuationKind)
    ) {
      throw new Error('session catalog response does not match the requested sort')
    }
    return snapshot
  }

  async liveSnapshot(sessionId: string, signal?: AbortSignal): Promise<WebSessionLiveSnapshot> {
    const response = await this.fetcher(`/api/sessions/${encodeURIComponent(sessionId)}/live`, {
      credentials: 'same-origin',
      headers: { accept: 'application/json' },
      signal,
    })
    const snapshot = decodeWebSessionLiveSnapshot(
      await readBoundedJson(response, this.maxJsonBodyBytes),
    )
    if (snapshot.session_id !== sessionId) {
      throw new Error('session live snapshot identity does not match the requested session')
    }
    return snapshot
  }

  async *attentionFollow(signal?: AbortSignal): AsyncGenerator<WebAttentionStreamEvent> {
    const response = await this.fetcher('/api/attention/follow', {
      credentials: 'same-origin',
      headers: { accept: 'application/x-ndjson' },
      signal,
    })
    yield* readBoundedNdjson(response, decodeWebAttentionStreamEvent, this.maxNdjsonRecordBytes)
  }

  async *sessionFollow(
    sessionId: string,
    signal?: AbortSignal,
  ): AsyncGenerator<WebSessionLiveStreamEvent> {
    const response = await this.fetcher(`/api/sessions/${encodeURIComponent(sessionId)}/follow`, {
      credentials: 'same-origin',
      headers: { accept: 'application/x-ndjson' },
      signal,
    })
    yield* readBoundedNdjson(response, decodeWebSessionLiveStreamEvent, this.maxNdjsonRecordBytes)
  }
}

const retryDelay = (signal: AbortSignal) =>
  new Promise<void>((resolve) => {
    const finish = () => {
      window.clearTimeout(timer)
      signal.removeEventListener('abort', finish)
      resolve()
    }
    const timer = window.setTimeout(finish, 250)
    signal.addEventListener('abort', finish, { once: true })
  })

export class SessionProjectionSynchronizer {
  constructor(private readonly source: HttpSessionProjectionSource) {}

  followAttention(
    onEvent: (event: WebAttentionStreamEvent) => void,
    onConnection: (state: FollowConnectionState) => void,
  ): () => void {
    return this.follow((signal) => this.source.attentionFollow(signal), onEvent, onConnection)
  }

  followSession(
    sessionId: string,
    onEvent: (event: WebSessionLiveStreamEvent) => void,
    onConnection: (state: FollowConnectionState) => void,
  ): () => void {
    return this.follow(
      (signal) => this.source.sessionFollow(sessionId, signal),
      onEvent,
      onConnection,
    )
  }

  private follow<T extends { kind: string }>(
    open: (signal: AbortSignal) => AsyncGenerator<T>,
    onEvent: (event: T) => void,
    onConnection: (state: FollowConnectionState) => void,
  ): () => void {
    const controller = new AbortController()
    const run = async () => {
      let reachedLive = false
      while (!controller.signal.aborted) {
        if (!reachedLive) onConnection('connecting')
        try {
          for await (const event of open(controller.signal)) {
            reachedLive = true
            onConnection('live')
            onEvent(event)
            if (event.kind === 'resync_required') break
          }
        } catch {
          if (controller.signal.aborted) return
        }
        if (controller.signal.aborted) return
        reachedLive = false
        onConnection('retrying')
        await retryDelay(controller.signal)
      }
    }
    void run()
    return () => controller.abort()
  }
}

const appendBytes = (
  left: Uint8Array<ArrayBufferLike>,
  right: Uint8Array<ArrayBufferLike>,
): Uint8Array<ArrayBufferLike> => {
  const joined = new Uint8Array(left.byteLength + right.byteLength)
  joined.set(left)
  joined.set(right, left.byteLength)
  return joined
}

const decodeLine = <T>(line: Uint8Array, decode: (value: unknown) => T): T => {
  if (line.byteLength === 0) throw new Error('NDJSON stream contains an empty record')
  return decode(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(line)))
}

export async function* readBoundedNdjson<T>(
  response: Response,
  decode: (value: unknown) => T,
  maxRecordBytes = MAX_NDJSON_RECORD_BYTES,
): AsyncGenerator<T> {
  if (!response.ok) throw new Error(`session follow failed with status ${response.status}`)
  if (!response.body) throw new Error('session follow response has no body')
  const reader = response.body.getReader()
  let pending: Uint8Array<ArrayBufferLike> = new Uint8Array()
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      let lineStart = 0
      for (let index = 0; index < value.byteLength; index += 1) {
        if (value[index] !== 10) continue
        const segment = value.subarray(lineStart, index)
        if (pending.byteLength + segment.byteLength > maxRecordBytes) {
          throw new Error('NDJSON record exceeds the byte limit')
        }
        const line = pending.byteLength === 0 ? segment : appendBytes(pending, segment)
        yield decodeLine(line, decode)
        pending = new Uint8Array()
        lineStart = index + 1
      }
      const trailing = value.subarray(lineStart)
      if (pending.byteLength + trailing.byteLength > maxRecordBytes) {
        throw new Error('NDJSON record exceeds the byte limit')
      }
      if (trailing.byteLength > 0) pending = appendBytes(pending, trailing)
    }
    if (pending.byteLength > 0) yield decodeLine(pending, decode)
  } finally {
    try {
      await reader.cancel()
    } catch {
      // The transport may already be closed while the bounded reader unwinds.
    }
    reader.releaseLock()
  }
}

const uniqueSummaries = (
  summaries: WebAttentionSnapshot['summaries'],
): WebAttentionSnapshot['summaries'] => {
  const bySession = new Map(summaries.map((summary) => [summary.session_id, summary]))
  return [...bySession.values()].slice(0, MAX_CATALOG_ROWS)
}

export const replaceCatalog = (snapshot: WebAttentionSnapshot): CatalogPresentation => ({
  snapshot,
  summaries: uniqueSummaries(snapshot.summaries),
})

/**
 * True when installing a snapshot at [incomingCursor] would replace an already
 * installed presentation with an older attention projection, which the follow
 * stream will never replay.
 */
export const catalogSnapshotRegresses = (
  current: CatalogPresentation,
  incomingCursor: string,
): boolean => current.snapshot !== null && BigInt(incomingCursor) < BigInt(current.snapshot.cursor)

/**
 * True when the live snapshot has observed durable events beyond the loaded
 * historical descriptor: events committed between the history read and the
 * follow subscription are folded into the initial live snapshot and are never
 * emitted as later durable records, so history must be refreshed to include
 * them.
 */
export const liveSnapshotOutrunsHistory = (
  historyObservedThrough: string | undefined,
  liveObservedThrough: string | undefined,
): boolean =>
  historyObservedThrough !== undefined &&
  liveObservedThrough !== undefined &&
  BigInt(liveObservedThrough) > BigInt(historyObservedThrough)

export const appendCatalog = (
  current: CatalogPresentation,
  page: WebAttentionSnapshot,
): CatalogPresentation => ({
  snapshot: page,
  summaries: uniqueSummaries([...current.summaries, ...page.summaries]),
})

export const applyAttentionEvent = (
  current: CatalogPresentation,
  event: WebAttentionStreamEvent,
): CatalogPresentation => {
  if (event.kind === 'resync_required') return current
  const updates = event.kind === 'snapshot' ? event.snapshot.summaries : event.summaries
  const bySession = new Map(updates.map((summary) => [summary.session_id, summary]))
  return {
    ...current,
    summaries: current.summaries.map((summary) => bySession.get(summary.session_id) ?? summary),
  }
}

const draftKey = (event: Extract<WebSessionLiveStreamEvent, { kind: 'provider_text_delta' }>) =>
  `${event.turn_id}:${event.model_call_id}:${event.part_index}`

const draftBytes = (drafts: ReadonlyArray<ProviderDraft>) =>
  drafts.reduce((total, draft) => total + new TextEncoder().encode(draft.content).byteLength, 0)

export const beginLiveResync = (current: LivePresentation): LivePresentation => ({
  ...current,
  durable: [],
  drafts: [],
  durableGap: null,
  resyncing: true,
})

export const applyLiveEvent = (
  current: LivePresentation,
  event: WebSessionLiveStreamEvent,
  expectedSessionId: string,
  historicalEventSequences?: ReadonlySet<string>,
): LivePresentation => {
  if (event.kind === 'snapshot') {
    if (event.snapshot.session_id !== expectedSessionId) {
      throw new Error('session live snapshot identity does not match the selected session')
    }
    const observedThrough = BigInt(event.snapshot.observed_through)
    if (current.snapshot !== null && observedThrough < BigInt(current.snapshot.observed_through)) {
      throw new Error('session live snapshot cursor regressed')
    }
    const durable = current.durable.filter(
      (item) =>
        BigInt(item.address.event_sequence) > observedThrough ||
        (historicalEventSequences !== undefined &&
          !historicalEventSequences.has(item.address.event_sequence)),
    )
    return {
      snapshot: event.snapshot,
      durable,
      drafts: [],
      durableGap: historyCoversDurableGap(current.durableGap, historicalEventSequences)
        ? null
        : current.durableGap,
      resyncing: false,
    }
  }
  if (event.kind === 'resync_required') {
    return beginLiveResync(current)
  }
  if (event.kind === 'durable') {
    if (current.snapshot === null) {
      throw new Error('session live durable event arrived before the initial snapshot')
    }
    const cursor = BigInt(event.cursor)
    if (event.cursor !== event.address.event_sequence) {
      throw new Error('session live durable cursor does not match its timeline address')
    }
    const previousCursor = BigInt(
      current.durable.at(-1)?.cursor ?? current.snapshot.observed_through,
    )
    if (cursor <= previousCursor) {
      throw new Error('session live durable cursor did not advance monotonically')
    }
    const withoutDuplicate = current.durable.filter(
      (item) => item.address.event_sequence !== event.address.event_sequence,
    )
    const durable = [...withoutDuplicate, event]
    const evicted = durable.length > MAX_LIVE_DURABLE_ITEMS ? durable[0] : undefined
    return {
      ...current,
      durable: durable.slice(-MAX_LIVE_DURABLE_ITEMS),
      durableGap: evicted?.address.event_sequence ?? current.durableGap,
    }
  }
  if (current.snapshot === null) {
    throw new Error('session live provider delta arrived before the initial snapshot')
  }
  if (current.resyncing) {
    throw new Error('session live provider delta arrived while resynchronizing')
  }
  const key = draftKey(event)
  const existing = current.drafts.find((draft) => draft.key === key)
  const next: ProviderDraft = {
    key,
    turnId: event.turn_id,
    modelCallId: event.model_call_id,
    partIndex: event.part_index,
    content: `${existing?.content ?? ''}${event.content}`,
  }
  const drafts = [...current.drafts.filter((draft) => draft.key !== key), next]
  if (drafts.length > MAX_PROVIDER_DRAFT_PARTS || draftBytes(drafts) > MAX_PROVIDER_DRAFT_BYTES) {
    return beginLiveResync(current)
  }
  return { ...current, drafts }
}

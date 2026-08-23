import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebContractBootstrap,
  type WebAttentionSnapshot,
  type WebContractBootstrap,
} from './generated/web-contract.mjs'
import generatedBootstrap from './generated/web-contract-bootstrap.json' with { type: 'json' }

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

const MAX_UNSIGNED_64 = 18_446_744_073_709_551_615n
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const CANONICAL_UNSIGNED_INTEGER = /^(0|[1-9]\d*)$/

const admittedSessionIdentity = (value: unknown) =>
  typeof value === 'string' && CANONICAL_UUID.test(value) ? value : undefined

const admittedActivityCursor = (value: unknown) => {
  if (typeof value !== 'string' || !CANONICAL_UNSIGNED_INTEGER.test(value)) return undefined
  return BigInt(value) <= MAX_UNSIGNED_64 ? value : undefined
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
export const MAX_PRODUCT_HTTP_RESPONSE_BYTES = 64 * 1024
export const MAX_SESSION_SEARCH_BYTES = 1024
const MAX_SESSION_SUMMARY_SCALARS = 128

const isCanonicalUnsigned64 = (value: string) =>
  CANONICAL_UNSIGNED_INTEGER.test(value) && BigInt(value) <= MAX_UNSIGNED_64
const isCanonicalPositiveUnsigned64 = (value: string) =>
  /^[1-9]\d*$/.test(value) && BigInt(value) <= MAX_UNSIGNED_64

const expectedActionByState: Record<
  WebAttentionSnapshot['summaries'][number]['state'],
  WebAttentionSnapshot['summaries'][number]['action']
> = {
  active: null,
  queued: null,
  blocked: 'provide_goal_need',
  awaiting_approval: 'decide_approval',
  ambiguous: 'reconcile_turn',
  awaiting_reconciliation: 'reconcile_turn',
  runner_lost: 'restore_runner',
  idle: null,
}

const statesDerivedFromCurrentTurn = new Set<WebAttentionSnapshot['summaries'][number]['state']>([
  'active',
  'queued',
  'awaiting_approval',
  'ambiguous',
  'awaiting_reconciliation',
])

export const admittedSessionSearch = (value: unknown) => {
  if (typeof value === 'string' && value.indexOf(String.fromCharCode(0)) !== -1) {
    return undefined
  }
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0')) return undefined
  return new TextEncoder().encode(value).byteLength <= MAX_SESSION_SEARCH_BYTES ? value : undefined
}

const readBoundedJson = async (response: Response): Promise<unknown> => {
  const reader = response.body?.getReader()
  if (!reader) throw new TypeError('product HTTP response has no body')
  const chunks: Uint8Array[] = []
  let byteCount = 0
  while (true) {
    const next = await reader.read()
    if (next.done) break
    byteCount += next.value.byteLength
    if (byteCount > MAX_PRODUCT_HTTP_RESPONSE_BYTES) {
      await reader.cancel()
      throw new TypeError('product HTTP response exceeds its encoded byte ceiling')
    }
    chunks.push(next.value)
  }
  const encoded = new Uint8Array(byteCount)
  let offset = 0
  for (const chunk of chunks) {
    encoded.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(encoded)) as unknown
}

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
  if (
    !isCanonicalUnsigned64(page.cursor) ||
    !isCanonicalUnsigned64(page.total) ||
    BigInt(page.total) < BigInt(page.summaries.length)
  ) {
    throw new Error('session catalog response contains a contradictory numeric page field')
  }
  const sessionIdentities = new Set<string>()
  for (const summary of page.summaries) {
    if (!CANONICAL_UUID.test(summary.session_id)) {
      throw new Error('session catalog response contains a non-canonical session identity')
    }
    if (summary.current_turn_id != null && !CANONICAL_UUID.test(summary.current_turn_id)) {
      throw new Error('session catalog response contains a non-canonical current-turn identity')
    }
    if (statesDerivedFromCurrentTurn.has(summary.state) && summary.current_turn_id == null) {
      throw new Error('session catalog turn-derived state is missing its current-turn identity')
    }
    if (sessionIdentities.has(summary.session_id)) {
      throw new Error('session catalog response contains a duplicate session identity')
    }
    sessionIdentities.add(summary.session_id)
    if (!request.includeArchived && summary.archived) {
      throw new Error('session catalog response contains an excluded archived session')
    }
    if (
      request.search !== undefined &&
      !summary.title_truncated &&
      !summary.session_id.includes(request.search) &&
      !(summary.title_summary?.includes(request.search) ?? false)
    ) {
      throw new Error('session catalog response contains a row that contradicts the active search')
    }
    if (
      !isCanonicalUnsigned64(summary.active_turn_count) ||
      !isCanonicalUnsigned64(summary.queued_turn_count) ||
      !isCanonicalUnsigned64(summary.judge.actionable) ||
      !isCanonicalUnsigned64(summary.judge.completed) ||
      !isCanonicalUnsigned64(summary.judge.escalated) ||
      !isCanonicalUnsigned64(summary.judge.failed) ||
      (summary.goal_block != null && !isCanonicalPositiveUnsigned64(summary.goal_block.generation))
    ) {
      throw new Error('session catalog response contains a non-canonical numeric field')
    }
    if (summary.action !== expectedActionByState[summary.state]) {
      throw new Error('session catalog response contains a contradictory state and action')
    }
    if (summary.state === 'blocked' && summary.goal_block == null) {
      throw new Error('session catalog blocked row is missing blocked-goal evidence')
    }
    if (
      summary.goal_block != null &&
      summary.state !== 'blocked' &&
      summary.state !== 'runner_lost'
    ) {
      throw new Error(
        'session catalog response contains goal-block evidence for an unrelated state',
      )
    }
    const titleScalars =
      typeof summary.title_summary === 'string' ? Array.from(summary.title_summary).length : 0
    if (
      summary.title_truncated &&
      (summary.title_summary == null || titleScalars !== MAX_SESSION_SUMMARY_SCALARS)
    ) {
      throw new Error('session catalog response contains a contradictory title truncation flag')
    }
    if (
      titleScalars > MAX_SESSION_SUMMARY_SCALARS ||
      (summary.goal_block !== null &&
        summary.goal_block !== undefined &&
        Array.from(summary.goal_block.need_summary).length > MAX_SESSION_SUMMARY_SCALARS)
    ) {
      throw new Error('session catalog response exceeds a summary scalar ceiling')
    }
    const milliseconds = summary.last_activity.unix_milliseconds
    const numericMilliseconds = Number(milliseconds)
    if (
      !isCanonicalUnsigned64(milliseconds) ||
      !Number.isSafeInteger(numericMilliseconds) ||
      !Number.isFinite(new Date(numericMilliseconds).getTime())
    ) {
      throw new Error('session catalog activity timestamp is outside the JavaScript Date range')
    }
  }
  const first = page.summaries[0]
  if (
    request.sort === 'identity' &&
    request.afterSession !== undefined &&
    first !== undefined &&
    first.session_id <= request.afterSession
  ) {
    throw new Error('session catalog response precedes its identity continuation')
  }
  if (
    request.sort === 'activity' &&
    request.afterActivity !== undefined &&
    first !== undefined &&
    BigInt(first.last_activity.unix_milliseconds) > BigInt(request.afterActivity) / 1000n
  ) {
    throw new Error('session catalog response precedes its activity continuation')
  }
  if (
    request.sort === 'activity' &&
    request.afterActivity !== undefined &&
    request.afterSession !== undefined &&
    BigInt(request.afterActivity) % 1000n === 0n &&
    first !== undefined &&
    BigInt(first.last_activity.unix_milliseconds) === BigInt(request.afterActivity) / 1000n &&
    first.session_id <= request.afterSession
  ) {
    throw new Error('session catalog response repeats its exact activity continuation boundary')
  }
  for (let index = 1; index < page.summaries.length; index += 1) {
    const previous = page.summaries[index - 1]
    const current = page.summaries[index]
    if (!previous || !current) continue
    const violatesSort =
      request.sort === 'identity'
        ? previous.session_id >= current.session_id
        : BigInt(previous.last_activity.unix_milliseconds) <
          BigInt(current.last_activity.unix_milliseconds)
    if (violatesSort) {
      throw new Error(`session catalog rows contradict ${expectedSort}`)
    }
  }
  if (
    request.afterSession === undefined &&
    page.continuation === null &&
    BigInt(page.total) > BigInt(page.summaries.length)
  ) {
    throw new Error('session catalog response omits a required continuation')
  }
  if (page.continuation) {
    if (BigInt(page.total) <= BigInt(page.summaries.length)) {
      throw new Error('session catalog continuation contradicts the declared total')
    }
    const boundary = page.summaries.at(-1)
    if (!boundary || page.continuation.session_id !== boundary.session_id) {
      throw new Error('session catalog continuation does not match its returned boundary')
    }
    if (page.continuation.kind === 'last_activity') {
      const milliseconds = boundary.last_activity.unix_milliseconds
      const microseconds = page.continuation.unix_microseconds
      if (!/^(0|[1-9]\d*)$/.test(milliseconds) || !/^(0|[1-9]\d*)$/.test(microseconds)) {
        throw new Error('session catalog boundary activity is not canonical')
      }
      const millisecondFloor = BigInt(milliseconds) * 1000n
      const exactMicroseconds = BigInt(microseconds)
      if (exactMicroseconds < millisecondFloor || exactMicroseconds >= millisecondFloor + 1000n) {
        throw new Error('session catalog continuation does not match its returned boundary')
      }
    }
    if (page.summaries.length !== MAX_SESSION_PAGE_ITEMS) {
      throw new Error('session catalog continuation accompanies a partial page')
    }
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
    const bootstrap = decodeWebContractBootstrap(await readBoundedJson(response))
    if (
      bootstrap.contract.name !== generatedBootstrap.contract.name ||
      bootstrap.contract.version !== generatedBootstrap.contract.version
    ) {
      throw new Error('bootstrap carries an incompatible web contract')
    }
    if (!bootstrap.capabilities.bounded_json) {
      throw new Error('bootstrap does not provide bounded JSON responses')
    }
    if (bootstrap.limits.max_json_body_bytes !== MAX_PRODUCT_HTTP_RESPONSE_BYTES) {
      throw new Error('bootstrap JSON response ceiling contradicts the browser contract')
    }
    return bootstrap
  }

  async readSessions(
    request: ProductSessionRequest,
    signal?: AbortSignal,
  ): Promise<WebAttentionSnapshot> {
    if (request.search && admittedSessionSearch(request.search) === undefined) {
      throw new TypeError('session catalog search exceeds its contract bound')
    }
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
      const failure = decodeWebApiErrorResponse(await readBoundedJson(response))
      throw new ProductRequestError(failure.error.code, failure.error.kind, failure.error.message)
    }
    return validateSessionPage(decodeWebAttentionSnapshot(await readBoundedJson(response)), request)
  }
}

export const productTransport = new SameOriginProductTransport()

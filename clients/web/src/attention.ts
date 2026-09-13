import {
  decodeWebApiErrorResponse,
  decodeWebApprovalRequest,
  decodeWebCancelTurnRequest,
  decodeWebGoalRequest,
  decodeWebSessionActionRequest,
  type WebApprovalRequest,
  type WebAttentionSnapshot,
  type WebAttentionStreamEvent,
  type WebCancelTurnRequest,
  type WebGoalRequest,
  type WebSessionActionRequest,
} from './generated/web-contract.mjs'
import type { ProductTransport } from './product'
import { MAX_PRODUCT_JSON_BYTES, ProductRequestError } from './product'

export type SessionAction =
  | { kind: 'approval'; requestId: string; input: WebApprovalRequest }
  | { kind: 'cancel'; input: WebCancelTurnRequest }
  | { kind: 'set-goal'; input: WebGoalRequest }
  | { kind: 'clear-goal'; input: WebSessionActionRequest }

export async function submitSessionAction(sessionId: string, action: SessionAction): Promise<void> {
  const input =
    action.kind === 'approval'
      ? decodeWebApprovalRequest(action.input)
      : action.kind === 'cancel'
        ? decodeWebCancelTurnRequest(action.input)
        : action.kind === 'set-goal'
          ? decodeWebGoalRequest(action.input)
          : decodeWebSessionActionRequest(action.input)
  const suffix =
    action.kind === 'approval'
      ? `approvals/${encodeURIComponent(action.requestId)}`
      : action.kind === 'cancel'
        ? 'cancel'
        : 'goal'
  const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/${suffix}`, {
    method:
      action.kind === 'approval' || action.kind === 'cancel'
        ? 'POST'
        : action.kind === 'set-goal'
          ? 'PUT'
          : 'DELETE',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify(input),
  })
  if (response.status === 204) return
  if (response.ok) throw new Error('The action was not acknowledged. Retry the same action.')
  const reader = response.body?.getReader()
  if (!reader) throw new Error('The action response was empty.')
  const chunks: Uint8Array[] = []
  let size = 0
  try {
    while (true) {
      const result = await reader.read()
      if (result.done) break
      size += result.value.byteLength
      if (size > MAX_PRODUCT_JSON_BYTES)
        throw new Error('The action response exceeded the JSON limit.')
      chunks.push(result.value)
    }
  } finally {
    await reader.cancel().catch(() => undefined)
    reader.releaseLock()
  }
  const bytes = new Uint8Array(size)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  throw new ProductRequestError(
    response.status,
    decodeWebApiErrorResponse(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))),
  )
}

export type AttentionSyncPhase = 'idle' | 'connecting' | 'live' | 'resyncing' | 'stale' | 'failed'

export type AttentionReduction =
  | { kind: 'projection'; snapshot: WebAttentionSnapshot }
  | { kind: 'resync' }

export type AttentionProjectionAcceptance = {
  snapshot: WebAttentionSnapshot
  accepted: boolean
}

export const reduceAttentionEvent = (
  current: WebAttentionSnapshot | undefined,
  event: WebAttentionStreamEvent,
): AttentionReduction => {
  if (event.kind === 'snapshot') return { kind: 'projection', snapshot: event.snapshot }
  if (event.kind === 'resync_required' || !current) return { kind: 'resync' }
  if (event.cursor === current.cursor && event.summaries.length === 0) {
    return { kind: 'projection', snapshot: current }
  }
  if (BigInt(event.cursor) <= BigInt(current.cursor)) return { kind: 'resync' }
  if (event.summaries.length === 0) return { kind: 'resync' }

  const updateSessionIds = new Set(event.summaries.map((summary) => summary.session_id))
  if (updateSessionIds.size !== event.summaries.length) return { kind: 'resync' }
  const replacements = new Map(event.summaries.map((summary) => [summary.session_id, summary]))
  const knownSessionIds = new Set(current.summaries.map((summary) => summary.session_id))
  const continuation = current.continuation_after_session_id ?? null
  if (
    event.summaries.some(
      (summary) =>
        !knownSessionIds.has(summary.session_id) &&
        (continuation === null || summary.session_id <= continuation),
    )
  ) {
    return { kind: 'resync' }
  }
  return {
    kind: 'projection',
    snapshot: {
      ...current,
      cursor: event.cursor,
      summaries: current.summaries.map(
        (summary) => replacements.get(summary.session_id) ?? summary,
      ),
    },
  }
}

// Tunable effective ceiling: repeated resync notices stop after three immediate reconnects so a
// damaged projection cannot create an unbounded browser request loop.
const MAX_IMMEDIATE_RESYNCS = 3

export const synchronizeAttention = async ({
  transport,
  signal,
  onPhase,
  onProjection,
}: {
  transport: ProductTransport
  signal: AbortSignal
  onPhase: (phase: AttentionSyncPhase) => void
  onProjection: (snapshot: WebAttentionSnapshot) => AttentionProjectionAcceptance
}): Promise<void> => {
  let resyncs = 0
  let resyncCursorFloor: bigint | undefined
  let projection: WebAttentionSnapshot | undefined
  let phase: AttentionSyncPhase = 'idle'
  const transition = (next: AttentionSyncPhase) => {
    if (next === phase) return
    phase = next
    onPhase(next)
  }
  transition('connecting')

  try {
    while (!signal.aborted) {
      let restart = false
      let firstEvent = true
      for await (const event of transport.followAttention(signal)) {
        if (
          (firstEvent && event.kind !== 'snapshot') ||
          (!firstEvent && event.kind === 'snapshot')
        ) {
          transition('failed')
          return
        }
        firstEvent = false
        const reduction = reduceAttentionEvent(projection, event)
        // A projection below the last advertised resync cursor omits a known journal
        // interval, so it is never installed as authority.
        const belowResyncCursorFloor =
          reduction.kind === 'projection' &&
          resyncCursorFloor !== undefined &&
          BigInt(reduction.snapshot.cursor) < resyncCursorFloor
        if (reduction.kind === 'resync' || belowResyncCursorFloor) {
          if (event.kind !== 'snapshot') {
            const advertised = BigInt(event.cursor)
            if (resyncCursorFloor === undefined || advertised > resyncCursorFloor) {
              resyncCursorFloor = advertised
            }
          }
          resyncs += 1
          if (resyncs > MAX_IMMEDIATE_RESYNCS) {
            transition('failed')
            return
          }
          transition('resyncing')
          restart = true
          break
        }
        const acceptance = onProjection(reduction.snapshot)
        projection = acceptance.snapshot
        transition('live')
      }
      if (!restart) {
        if (!signal.aborted) transition('stale')
        return
      }
    }
  } catch {
    if (!signal.aborted) transition('failed')
  }
}

const canonicalProjection = (value: unknown): unknown => {
  if (Array.isArray(value)) return value.map(canonicalProjection)
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value)
        .filter(([, item]) => item != null)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, item]) => [key, canonicalProjection(item)]),
    )
  }
  return value
}

export const attentionSnapshotsMatch = (
  left: WebAttentionSnapshot,
  right: WebAttentionSnapshot,
): boolean =>
  JSON.stringify(canonicalProjection(left)) === JSON.stringify(canonicalProjection(right))

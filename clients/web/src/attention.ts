import type { WebAttentionSnapshot, WebAttentionStreamEvent } from './generated/web-contract.mjs'
import type { ProductTransport } from './product'

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
        const cursorBeforeReduction = projection?.cursor
        const reduction = reduceAttentionEvent(projection, event)
        if (reduction.kind === 'resync') {
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
        if (
          event.kind === 'update' &&
          acceptance.accepted &&
          cursorBeforeReduction !== undefined &&
          BigInt(acceptance.snapshot.cursor) > BigInt(cursorBeforeReduction)
        ) {
          resyncs = 0
        }
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

import type { QueryClient } from '@tanstack/react-query'
import { followSession, readSessionLive } from './product'
import { type AppDispatch, actions, type RootState, type SessionSyncState } from './state'

// Hard safety ceilings: retained transient provider text is bounded independently of history.
const MAX_PROVIDER_DRAFT_PARTS = 32
const MAX_PROVIDER_DRAFT_BYTES = 65_536

/** Owns the single selected session stream independently of React renders. */
export function startSessionSynchronization(
  store: {
    getState: () => RootState
    dispatch: AppDispatch
    subscribe: (listener: () => void) => () => void
  },
  queryClient: QueryClient,
) {
  let active: { sessionId: string | null; attempt: number; controller: AbortController } | null =
    null
  const changed = () => {
    const requested = store.getState().app.sessionSync
    if (active?.sessionId === requested.sessionId && active?.attempt === requested.attempt) return
    active?.controller.abort()
    const controller = new AbortController()
    active = { sessionId: requested.sessionId, attempt: requested.attempt, controller }
    const sessionId = requested.sessionId
    if (sessionId === null) return
    let projection: SessionSyncState = requested
    let retainedDraftBytes = projection.drafts.reduce(
      (total, draft) => total + new TextEncoder().encode(draft.content).byteLength,
      0,
    )
    const publish = (update: Partial<SessionSyncState>) => {
      if (controller.signal.aborted) return
      if (
        update.cursor !== undefined &&
        update.cursor !== null &&
        projection.cursor !== null &&
        BigInt(update.cursor) < BigInt(projection.cursor)
      )
        return
      if (update.drafts?.length === 0) retainedDraftBytes = 0
      projection = { ...projection, ...update }
      store.dispatch(actions.sessionFollowUpdated(projection))
    }
    const refresh = () =>
      queryClient.invalidateQueries({
        queryKey: ['production', 'session-workspace', sessionId],
        exact: true,
      })
    void (async () => {
      try {
        for await (const event of followSession(
          sessionId,
          controller.signal,
          () => projection.phase === 'resyncing',
        )) {
          if (controller.signal.aborted) return
          if (event.kind === 'snapshot') {
            publish({
              phase: 'live',
              snapshot: event.snapshot,
              cursor: event.snapshot.observed_through,
              drafts: [],
            })
            await refresh()
          } else if (event.kind === 'durable') {
            publish({ cursor: event.cursor })
            await refresh()
            const snapshot = await readSessionLive(sessionId, controller.signal)
            publish({ phase: 'live', snapshot, cursor: snapshot.observed_through, drafts: [] })
          } else if (event.kind === 'provider_text_delta') {
            const key = `${event.turn_id}:${event.model_call_id}:${event.part_index}`
            const existing = projection.drafts.find((draft) => draft.key === key)
            const bytes = retainedDraftBytes + new TextEncoder().encode(event.content).byteLength
            if (
              (!existing && projection.drafts.length === MAX_PROVIDER_DRAFT_PARTS) ||
              bytes > MAX_PROVIDER_DRAFT_BYTES
            ) {
              publish({ phase: 'resyncing', snapshot: null, drafts: [] })
              continue
            }
            retainedDraftBytes = bytes
            const next = { key, content: `${existing?.content ?? ''}${event.content}` }
            publish({
              drafts: existing
                ? projection.drafts.map((draft) => (draft.key === key ? next : draft))
                : [...projection.drafts, next],
            })
          } else if (event.kind === 'resync_required') {
            publish({ phase: 'resyncing', cursor: event.cursor, snapshot: null, drafts: [] })
          }
        }
      } catch {
        publish({ phase: 'failed', drafts: [] })
      }
    })()
  }
  const unsubscribe = store.subscribe(changed)
  changed()
  return () => {
    unsubscribe()
    active?.controller.abort()
  }
}

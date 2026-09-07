import type { QueryClient } from '@tanstack/react-query'
import { followSession, readSessionLive } from './product'
import { extendSessionWorkspace } from './session-workspace'
import { type AppDispatch, actions, type RootState, type SessionSyncState } from './state'

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
    const publish = (update: Partial<SessionSyncState>) => {
      if (controller.signal.aborted) return
      if (
        update.cursor !== undefined &&
        update.cursor !== null &&
        projection.cursor !== null &&
        BigInt(update.cursor) < BigInt(projection.cursor)
      )
        return
      projection = { ...projection, ...update }
      store.dispatch(actions.sessionFollowUpdated(projection))
    }
    const extendHistory = (cursor: string) =>
      extendSessionWorkspace(queryClient, sessionId, cursor, controller.signal).catch(
        () => undefined,
      )
    void (async () => {
      try {
        for await (const event of followSession(sessionId, controller.signal)) {
          if (controller.signal.aborted) return
          if (event.kind === 'snapshot') {
            publish({
              phase: 'live',
              snapshot: event.snapshot,
              cursor: event.snapshot.observed_through,
            })
            await extendHistory(event.snapshot.observed_through)
          } else if (event.kind === 'durable') {
            publish({ cursor: event.cursor })
            await extendHistory(event.cursor)
            const snapshot = await readSessionLive(sessionId, controller.signal)
            publish({ phase: 'live', snapshot, cursor: snapshot.observed_through })
          } else if (event.kind === 'resync_required') {
            publish({ phase: 'resyncing', cursor: event.cursor })
          }
        }
      } catch {
        publish({ phase: 'failed' })
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

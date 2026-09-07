import type { QueryClient } from '@tanstack/react-query'
import type { WebSessionLiveSnapshot } from './generated/web-contract.mjs'
import { followSession, readSessionLive } from './product'
import { extendSessionWorkspace } from './session-workspace'
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
    const drafts = new Map(
      projection.drafts.map((draft) => [
        draft.key,
        { fragments: [draft.content], bytes: new TextEncoder().encode(draft.content).byteLength },
      ]),
    )
    let retainedDraftBytes = [...drafts.values()].reduce((total, draft) => total + draft.bytes, 0)
    let draftFrame: number | null = null
    const cancelDraftFrame = () => {
      if (draftFrame !== null) cancelAnimationFrame(draftFrame)
      draftFrame = null
    }
    const clearDrafts = () => {
      cancelDraftFrame()
      drafts.clear()
      retainedDraftBytes = 0
    }
    controller.signal.addEventListener('abort', clearDrafts, { once: true })
    const staleCursor = (cursor: string) =>
      projection.cursor !== null && BigInt(cursor) < BigInt(projection.cursor)
    const publish = (update: Partial<SessionSyncState>) => {
      if (controller.signal.aborted) return
      if (update.cursor !== undefined && update.cursor !== null && staleCursor(update.cursor))
        return
      projection = { ...projection, ...update }
      store.dispatch(actions.sessionFollowUpdated(projection))
    }
    const draftProjection = () =>
      [...drafts].map(([key, draft]) => {
        const content = draft.fragments.join('')
        draft.fragments = [content]
        return { key, content }
      })
    const publishSnapshot = (snapshot: WebSessionLiveSnapshot, retainCorrelated: boolean) => {
      if (controller.signal.aborted || staleCursor(snapshot.observed_through)) return
      cancelDraftFrame()
      if (retainCorrelated) {
        const active = snapshot.active
        const prefix =
          active && 'model_call_id' in active.state && active.state.model_call_id !== null
            ? `${active.turn_id}:${active.state.model_call_id}:`
            : null
        for (const [key, draft] of drafts) {
          if (prefix === null || !key.startsWith(prefix)) {
            retainedDraftBytes -= draft.bytes
            drafts.delete(key)
          }
        }
      } else clearDrafts()
      publish({
        phase: 'live',
        snapshot,
        cursor: snapshot.observed_through,
        drafts: draftProjection(),
      })
    }
    let pendingHistoryCursor: string | null = null
    let extendingHistory = false
    const extendHistory = (cursor: string) => {
      if (pendingHistoryCursor === null || BigInt(cursor) > BigInt(pendingHistoryCursor))
        pendingHistoryCursor = cursor
      if (extendingHistory) return
      extendingHistory = true
      void (async () => {
        while (!controller.signal.aborted && pendingHistoryCursor !== null) {
          const observed = pendingHistoryCursor
          pendingHistoryCursor = null
          await extendSessionWorkspace(queryClient, sessionId, observed, controller.signal).catch(
            () => undefined,
          )
        }
        extendingHistory = false
      })()
    }
    void (async () => {
      try {
        for await (const event of followSession(
          sessionId,
          controller.signal,
          () => projection.phase === 'resyncing',
        )) {
          if (controller.signal.aborted) return
          if (event.kind === 'snapshot') {
            publishSnapshot(event.snapshot, false)
            extendHistory(event.snapshot.observed_through)
          } else if (event.kind === 'durable') {
            publish({ cursor: event.cursor })
            extendHistory(event.cursor)
            const snapshot = await readSessionLive(sessionId, controller.signal)
            publishSnapshot(snapshot, true)
          } else if (event.kind === 'provider_text_delta') {
            if (event.content.length === 0) continue
            const activeCall = projection.snapshot?.active
            if (
              activeCall?.state.kind !== 'running' ||
              activeCall.turn_id !== event.turn_id ||
              activeCall.state.model_call_id !== event.model_call_id
            ) {
              clearDrafts()
              publish({ phase: 'resyncing', snapshot: null, drafts: [] })
              continue
            }
            const key = `${event.turn_id}:${event.model_call_id}:${event.part_index}`
            const existing = drafts.get(key)
            const bytes = new TextEncoder().encode(event.content).byteLength
            if (
              (!existing && drafts.size === MAX_PROVIDER_DRAFT_PARTS) ||
              retainedDraftBytes + bytes > MAX_PROVIDER_DRAFT_BYTES
            ) {
              clearDrafts()
              publish({ phase: 'resyncing', snapshot: null, drafts: [] })
              continue
            }
            retainedDraftBytes += bytes
            if (existing) {
              existing.fragments.push(event.content)
              existing.bytes += bytes
            } else drafts.set(key, { fragments: [event.content], bytes })
            if (draftFrame === null)
              draftFrame = requestAnimationFrame(() => {
                draftFrame = null
                publish({ drafts: draftProjection() })
              })
          } else if (event.kind === 'resync_required') {
            clearDrafts()
            publish({
              phase: 'resyncing',
              cursor: staleCursor(event.cursor) ? projection.cursor : event.cursor,
              snapshot: null,
              drafts: [],
            })
          }
        }
      } catch {
        clearDrafts()
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

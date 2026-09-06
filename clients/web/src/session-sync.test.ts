import { QueryClient } from '@tanstack/react-query'
import { afterEach, expect, it, vi } from 'vitest'
import type {
  WebSessionLiveSnapshot,
  WebSessionLiveStreamEvent,
} from './generated/web-contract.mjs'
import { followSession } from './product'
import { startSessionSynchronization } from './session-sync'
import { actions, createAppStore, selectSessionSync } from './state'

vi.mock('./product', () => ({ followSession: vi.fn(), readSessionLive: vi.fn() }))
afterEach(() => vi.resetAllMocks())

const snapshot = (sessionId: string): WebSessionLiveSnapshot => ({
  session_id: sessionId,
  observed_through: '41',
  active: null,
  queued_turn_count: '0',
  queued_turn_ids: [],
  reconciliation: null,
  runner: null,
})

it('keeps one stream across unrelated state changes and rejects an obsolete stream on navigation', async () => {
  const first = '00000000-0000-0000-0000-000000000991'
  const second = '00000000-0000-0000-0000-000000000992'
  let releaseFirst = () => {}
  const waiting = new Promise<void>((resolve) => {
    releaseFirst = resolve
  })
  const signals: AbortSignal[] = []
  vi.mocked(followSession).mockImplementation(async function* (sessionId, signal) {
    signals.push(signal)
    yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
    if (sessionId === first) {
      await waiting
      yield { kind: 'resync_required', cursor: '99' }
    } else {
      yield { kind: 'resync_required', cursor: '42' }
      throw new Error('fixture stream disconnected')
    }
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const refreshed = vi.spyOn(queries, 'invalidateQueries')
  const phases: string[] = []
  const unsubscribe = store.subscribe(() => phases.push(selectSessionSync(store.getState()).phase))
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(first))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('live'))
  store.dispatch(actions.timelineSelected('41'))
  expect(followSession).toHaveBeenCalledTimes(1)
  expect(refreshed).toHaveBeenCalledWith({
    queryKey: ['production', 'session-workspace', first],
    exact: true,
  })
  store.dispatch(actions.sessionFollowRequested(second))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('failed'))
  expect(phases).toContain('resyncing')
  expect(selectSessionSync(store.getState())).toMatchObject({ sessionId: second, cursor: '42' })
  expect(signals[0]?.aborted).toBe(true)
  releaseFirst()
  await new Promise((resolve) => setTimeout(resolve, 0))
  expect(selectSessionSync(store.getState())).toMatchObject({ sessionId: second, cursor: '42' })
  stop()
  expect(signals[1]?.aborted).toBe(true)
  unsubscribe()
  queries.clear()
})

it('records explicit reconnects as a new session synchronization attempt', async () => {
  vi.mocked(followSession).mockImplementation(
    async function* (sessionId): AsyncGenerator<WebSessionLiveStreamEvent> {
      yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
      throw new Error('fixture stream unavailable')
    },
  )
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested('00000000-0000-0000-0000-000000000991'))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('failed'))
  const attempt = selectSessionSync(store.getState()).attempt
  store.dispatch(actions.sessionFollowReconnectRequested())
  await vi.waitFor(() => expect(followSession).toHaveBeenCalledTimes(2))
  expect(selectSessionSync(store.getState()).attempt).toBe(attempt + 1)
  stop()
  queries.clear()
})

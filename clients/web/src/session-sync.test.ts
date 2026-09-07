import { QueryClient } from '@tanstack/react-query'
import { afterEach, expect, it, vi } from 'vitest'
import type {
  WebSessionLiveSnapshot,
  WebSessionLiveStreamEvent,
} from './generated/web-contract.mjs'
import { followSession, readSessionLive } from './product'
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

it('retains the newer live cursor when a buffered event is followed by a failed live read', async () => {
  const sessionId = '00000000-0000-0000-0000-000000000991'
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
    for (const cursor of ['42', '43'])
      yield {
        kind: 'durable',
        cursor,
        address: { event_sequence: cursor },
        event_kind: 'input_accepted',
      }
  })
  vi.mocked(readSessionLive)
    .mockResolvedValueOnce({ ...snapshot(sessionId), observed_through: '50' })
    .mockRejectedValueOnce(new Error('fixture live read failed'))
  const store = createAppStore()
  const queries = new QueryClient()
  const cursors: string[] = []
  const unsubscribe = store.subscribe(() => {
    const cursor = selectSessionSync(store.getState()).cursor
    if (cursor !== null) cursors.push(cursor)
  })
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(sessionId))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('failed'))
  expect(selectSessionSync(store.getState())).toMatchObject({
    cursor: '50',
    snapshot: { observed_through: '50' },
  })
  expect(cursors).not.toContain('43')
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
    throw new Error('fixture reconnect failed')
  })
  store.dispatch(actions.sessionFollowReconnectRequested())
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('failed'))
  expect(selectSessionSync(store.getState()).cursor).toBe('50')
  expect(
    cursors.every(
      (cursor, index) => index === 0 || BigInt(cursor) >= BigInt(cursors[index - 1] ?? '0'),
    ),
  ).toBe(true)
  stop()
  unsubscribe()
  queries.clear()
})

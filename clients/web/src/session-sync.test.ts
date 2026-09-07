import { QueryClient } from '@tanstack/react-query'
import { afterEach, expect, it, vi } from 'vitest'
import type {
  WebSessionLiveSnapshot,
  WebSessionLiveStreamEvent,
} from './generated/web-contract.mjs'
import { followSession, readSessionLive } from './product'
import { startSessionSynchronization } from './session-sync'
import { BoundedSessionHistory } from './session-timeline/model'
import type { SessionWorkspace } from './session-workspace'
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
  expect(refreshed).not.toHaveBeenCalled()
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

it('preserves held history across ordinary and resynchronization snapshots', async () => {
  const sessionId = '00000000-0000-0000-0000-000000000991'
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
    yield { kind: 'snapshot', snapshot: { ...snapshot(sessionId), observed_through: '42' } }
    yield { kind: 'resync_required', cursor: '43' }
    yield { kind: 'snapshot', snapshot: { ...snapshot(sessionId), observed_through: '43' } }
    yield { kind: 'snapshot', snapshot: { ...snapshot(sessionId), observed_through: '44' } }
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const refreshed = vi.spyOn(queries, 'invalidateQueries')
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(sessionId))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).cursor).toBe('44'))
  expect(refreshed).not.toHaveBeenCalled()
  stop()
  queries.clear()
})

it('continues processing live snapshots when an in-flight historical read fails', async () => {
  const sessionId = '00000000-0000-0000-0000-000000000991'
  const queries = new QueryClient()
  let failHistory = () => {}
  const historyRead = queries.fetchQuery({
    queryKey: ['production', 'session-workspace', sessionId],
    queryFn: () =>
      new Promise((_, reject) => {
        failHistory = () => reject(new Error('history unavailable'))
      }),
    retry: false,
  })
  void historyRead.catch(() => undefined)
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
    yield { kind: 'snapshot', snapshot: { ...snapshot(sessionId), observed_through: '42' } }
  })
  const store = createAppStore()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(sessionId))
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).phase).toBe('live'))
  failHistory()
  await expect(historyRead).rejects.toThrow('history unavailable')
  await vi.waitFor(() =>
    expect(selectSessionSync(store.getState())).toMatchObject({ phase: 'live', cursor: '42' }),
  )
  stop()
  queries.clear()
})

it.each(['describe', 'load'] as const)(
  'continues live following and retries history after an extension %s failure',
  async (failure) => {
    const sessionId = '00000000-0000-0000-0000-000000000991'
    const descriptor = {
      session_id: sessionId,
      observed_through: '42',
      first_address: { event_sequence: '40' },
      latest_address: { event_sequence: '42' },
      sizes: {
        item_count: '3',
        projected_structured_bytes: '234',
        projected_text_bytes: '0',
        referenced_blob_count: '0',
        referenced_blob_bytes: '0',
      },
      work: { active_turn_count: '0', queued_turn_count: '0' },
    }
    const item = (sequence: string) => ({
      address: { event_sequence: sequence },
      kind: 'input_accepted' as const,
      projected_structured_bytes: 78,
    })
    const source = {
      limits: { max_timeline_window_items: 256, max_timeline_window_bytes: 65_536 },
      readDescriptor: vi.fn().mockResolvedValue(descriptor),
      readWindow: vi.fn().mockResolvedValue({
        session_id: sessionId,
        items: [item('41'), item('42')],
        projected_structured_bytes: 156,
        continuation_before: { event_sequence: '41' },
        continuation_after: null,
      }),
    }
    const failedRead = failure === 'describe' ? source.readDescriptor : source.readWindow
    failedRead.mockRejectedValueOnce(new Error('history temporarily unavailable'))
    const queries = new QueryClient()
    const key = ['production', 'session-workspace', sessionId]
    queries.setQueryData<SessionWorkspace>(key, {
      active: false,
      anchor: { kind: 'latest' },
      descriptor: {
        ...descriptor,
        observed_through: '40',
        latest_address: { event_sequence: '40' },
        sizes: { ...descriptor.sizes, item_count: '1', projected_structured_bytes: '78' },
      },
      history: new BoundedSessionHistory(sessionId, source),
      window: {
        session_id: sessionId,
        items: [item('40')],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: null,
      },
    })
    vi.mocked(followSession).mockImplementation(async function* () {
      yield { kind: 'snapshot', snapshot: snapshot(sessionId) }
      yield {
        kind: 'durable',
        cursor: '42',
        address: { event_sequence: '42' },
        event_kind: 'input_accepted',
      }
      yield { kind: 'snapshot', snapshot: { ...snapshot(sessionId), observed_through: '43' } }
    })
    vi.mocked(readSessionLive).mockResolvedValue({ ...snapshot(sessionId), observed_through: '42' })
    const store = createAppStore()
    const stop = startSessionSynchronization(store, queries)
    store.dispatch(actions.sessionFollowRequested(sessionId))
    await vi.waitFor(() =>
      expect(selectSessionSync(store.getState())).toMatchObject({ phase: 'live', cursor: '43' }),
    )
    expect(queries.getQueryData<SessionWorkspace>(key)?.window.items).toEqual([
      item('40'),
      item('41'),
      item('42'),
    ])
    expect(failedRead.mock.calls.length).toBeGreaterThanOrEqual(2)
    stop()
    queries.clear()
  },
)

import { QueryClient } from '@tanstack/react-query'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
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
beforeEach(() => {
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) =>
    setTimeout(() => callback(0), 0),
  )
  vi.stubGlobal('cancelAnimationFrame', clearTimeout)
})
afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetAllMocks()
})

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

const draftSessionId = '00000000-0000-0000-0000-000000000993'
const runningSnapshot = (sessionId = draftSessionId): WebSessionLiveSnapshot => ({
  ...snapshot(sessionId),
  active: { turn_id: sessionId, state: { kind: 'running', model_call_id: sessionId } },
})
const draft = (content: string, part_index = 0): WebSessionLiveStreamEvent => ({
  kind: 'provider_text_delta',
  turn_id: draftSessionId,
  model_call_id: draftSessionId,
  part_index,
  content,
})

it('joins provider parts in arrival order and discards them on a replacement snapshot', async () => {
  let replace = () => {}
  const replacement = new Promise<void>((resolve) => {
    replace = resolve
  })
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    yield draft('First')
    yield draft('Second', 1)
    yield draft(' part')
    await replacement
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() =>
    expect(selectSessionSync(store.getState()).drafts.map((part) => part.content)).toEqual([
      'First part',
      'Second',
    ]),
  )
  replace()
  await vi.waitFor(() => expect(selectSessionSync(store.getState()).drafts).toEqual([]))
  stop()
  queries.clear()
})

it.each([
  { limit: 'part count', events: Array.from({ length: 33 }, (_, index) => draft('x', index)) },
  { limit: 'UTF-8 bytes', events: [draft('é'.repeat(32_768)), draft('x')] },
])('requests resynchronization and clears every draft on $limit overflow', async ({ events }) => {
  let requested = false
  vi.mocked(followSession).mockImplementation(async function* (_sessionId, _signal, needsResync) {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    for (const event of events) yield event
    requested = needsResync?.() ?? false
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const retained: { parts: number; bytes: number }[] = []
  const unsubscribe = store.subscribe(() => {
    const drafts = selectSessionSync(store.getState()).drafts
    retained.push({
      parts: drafts.length,
      bytes: drafts.reduce((sum, part) => sum + new TextEncoder().encode(part.content).length, 0),
    })
  })
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() => expect(requested).toBe(true))
  expect(selectSessionSync(store.getState())).toMatchObject({
    phase: 'resyncing',
    snapshot: null,
    drafts: [],
  })
  expect(Math.max(...retained.map((value) => value.parts))).toBeLessThanOrEqual(32)
  expect(Math.max(...retained.map((value) => value.bytes))).toBeLessThanOrEqual(65_536)
  stop()
  unsubscribe()
  queries.clear()
})

it('accounts for streamed bytes without encoding accumulated draft text again', async () => {
  const fragmentCount = 4096
  const replacedDraftBytes = 65_536
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    yield draft('x'.repeat(replacedDraftBytes))
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    for (let index = 0; index < fragmentCount; index++) yield draft('x')
  })
  const encode = vi.spyOn(TextEncoder.prototype, 'encode')
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() =>
    expect(selectSessionSync(store.getState()).drafts[0]?.content.length).toBe(fragmentCount),
  )
  expect(encode.mock.calls.reduce((sum, [text]) => sum + (text?.length ?? 0), 0)).toBe(
    replacedDraftBytes + fragmentCount,
  )
  encode.mockRestore()
  stop()
  queries.clear()
})

it('keeps the streamed prefix across a durable update for the same active call', async () => {
  const live = {
    ...runningSnapshot(),
    active: {
      turn_id: draftSessionId,
      state: { kind: 'running' as const, model_call_id: draftSessionId },
    },
  }
  vi.mocked(readSessionLive).mockResolvedValue({ ...live, observed_through: '42' })
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: live }
    yield draft('Prefix ')
    yield {
      kind: 'durable',
      cursor: '42',
      address: { event_sequence: '42' },
      event_kind: 'input_accepted',
    }
    yield draft('suffix')
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() =>
    expect(selectSessionSync(store.getState()).drafts[0]?.content).toBe('Prefix suffix'),
  )
  stop()
  queries.clear()
})

it('publishes a full byte budget of one-byte fragments as one complete display update', async () => {
  const fragmentCount = 65_536
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    for (let index = 0; index < fragmentCount; index++) yield draft('x')
  })
  const store = createAppStore()
  const queries = new QueryClient()
  let draftPublications = 0
  const unsubscribe = store.subscribe(() => {
    if (selectSessionSync(store.getState()).drafts.length > 0) draftPublications++
  })
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() =>
    expect(selectSessionSync(store.getState()).drafts[0]?.content).toBe('x'.repeat(fragmentCount)),
  )
  expect(draftPublications).toBe(1)
  stop()
  unsubscribe()
  queries.clear()
})

it.each(['turn', 'model call', 'completion'] as const)(
  'discards drafts when a durable update changes the active %s',
  async (change) => {
    const otherId = '00000000-0000-0000-0000-000000000994'
    const live = {
      ...runningSnapshot(),
      observed_through: '42',
      active:
        change === 'completion'
          ? null
          : {
              turn_id: change === 'turn' ? otherId : draftSessionId,
              state: {
                kind: 'running' as const,
                model_call_id: change === 'model call' ? otherId : draftSessionId,
              },
            },
    }
    vi.mocked(readSessionLive).mockResolvedValue(live)
    vi.mocked(followSession).mockImplementation(async function* () {
      yield { kind: 'snapshot', snapshot: runningSnapshot() }
      yield draft('Old call')
      yield {
        kind: 'durable',
        cursor: '42',
        address: { event_sequence: '42' },
        event_kind: 'input_accepted',
      }
    })
    const store = createAppStore()
    const queries = new QueryClient()
    const stop = startSessionSynchronization(store, queries)
    store.dispatch(actions.sessionFollowRequested(draftSessionId))
    await vi.waitFor(() => expect(selectSessionSync(store.getState()).snapshot).toEqual(live))
    expect(selectSessionSync(store.getState()).drafts).toEqual([])
    stop()
    queries.clear()
  },
)

it('keeps retained byte accounting when a durable update preserves the active call', async () => {
  const live = {
    ...runningSnapshot(),
    active: {
      turn_id: draftSessionId,
      state: { kind: 'running' as const, model_call_id: draftSessionId },
    },
  }
  let requested = false
  vi.mocked(readSessionLive).mockResolvedValue({ ...live, observed_through: '42' })
  vi.mocked(followSession).mockImplementation(async function* (_sessionId, _signal, needsResync) {
    yield { kind: 'snapshot', snapshot: live }
    yield draft('é'.repeat(32_768))
    yield {
      kind: 'durable',
      cursor: '42',
      address: { event_sequence: '42' },
      event_kind: 'input_accepted',
    }
    yield draft('x')
    requested = needsResync?.() ?? false
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await vi.waitFor(() => expect(requested).toBe(true))
  expect(selectSessionSync(store.getState())).toMatchObject({ phase: 'resyncing', drafts: [] })
  stop()
  queries.clear()
})

it('cancels a pending draft publication when leaving the session', async () => {
  let queued = () => {}
  const queuedDraft = new Promise<void>((resolve) => {
    queued = resolve
  })
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    yield draft('Pending')
    queued()
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  await queuedDraft
  store.dispatch(actions.sessionFollowRequested(null))
  let publications = 0
  const unsubscribe = store.subscribe(() => {
    publications++
  })
  await new Promise((resolve) => requestAnimationFrame(resolve))
  expect(publications).toBe(0)
  expect(selectSessionSync(store.getState())).toMatchObject({ sessionId: null, drafts: [] })
  stop()
  unsubscribe()
  queries.clear()
})

it.each([
  { branch: 'snapshot', stalledRead: 'describe' },
  { branch: 'snapshot', stalledRead: 'load' },
  { branch: 'durable', stalledRead: 'describe' },
  { branch: 'durable', stalledRead: 'load' },
] as const)(
  'keeps live events moving while the $branch history $stalledRead stalls',
  async ({ branch, stalledRead }) => {
    const sessionId = draftSessionId
    const extensions = vi.spyOn(await import('./session-workspace'), 'extendSessionWorkspace')
    const heldThrough = branch === 'snapshot' ? 40 : 41
    const item = (sequence: number) => ({
      address: { event_sequence: String(sequence) },
      kind: 'input_accepted' as const,
      projected_structured_bytes: 78,
    })
    const descriptor = (through: number, observed = through) => ({
      session_id: sessionId,
      observed_through: String(observed),
      first_address: { event_sequence: '40' },
      latest_address: { event_sequence: String(through) },
      sizes: {
        item_count: String(through - 39),
        projected_structured_bytes: String((through - 39) * 78),
        projected_text_bytes: '0',
        referenced_blob_count: '0',
        referenced_blob_bytes: '0',
      },
      work: { active_turn_count: '0', queued_turn_count: '0' },
    })
    let release = () => {}
    let started = () => {}
    const stalled = new Promise<void>((resolve) => {
      release = resolve
    })
    const historyStarted = new Promise<void>((resolve) => {
      started = resolve
    })
    const source = {
      limits: { max_timeline_window_items: 256, max_timeline_window_bytes: 65_536 },
      readDescriptor: vi
        .fn()
        .mockResolvedValue(descriptor(42, 1042))
        .mockImplementationOnce(async () => {
          if (stalledRead === 'describe') {
            started()
            await stalled
          }
          return descriptor(42)
        }),
      readWindow: vi.fn().mockImplementation(async () => {
        if (stalledRead === 'load') {
          started()
          await stalled
        }
        return {
          session_id: sessionId,
          items: Array.from({ length: 42 - heldThrough }, (_, index) =>
            item(heldThrough + index + 1),
          ),
          projected_structured_bytes: (42 - heldThrough) * 78,
          continuation_before: { event_sequence: String(heldThrough + 1) },
          continuation_after: null,
        }
      }),
    }
    const queries = new QueryClient()
    const key = ['production', 'session-workspace', sessionId]
    queries.setQueryData<SessionWorkspace>(key, {
      active: false,
      anchor: { kind: 'latest' },
      descriptor: descriptor(heldThrough),
      history: new BoundedSessionHistory(sessionId, source),
      window: {
        session_id: sessionId,
        items: Array.from({ length: heldThrough - 39 }, (_, index) => item(40 + index)),
        projected_structured_bytes: (heldThrough - 39) * 78,
        continuation_before: null,
        continuation_after: null,
      },
    })
    vi.mocked(followSession).mockImplementation(async function* () {
      yield { kind: 'snapshot', snapshot: runningSnapshot(sessionId) }
      yield {
        kind: 'durable',
        cursor: '42',
        address: { event_sequence: '42' },
        event_kind: 'input_accepted',
      }
      await historyStarted
      for (let cursor = 43; cursor <= 1042; cursor++) {
        yield {
          kind: 'snapshot',
          snapshot: { ...runningSnapshot(sessionId), observed_through: String(cursor) },
        }
      }
      yield draft('Text while history stalls')
    })
    vi.mocked(readSessionLive).mockResolvedValue({
      ...runningSnapshot(sessionId),
      observed_through: '42',
    })
    const store = createAppStore()
    const stop = startSessionSynchronization(store, queries)
    store.dispatch(actions.sessionFollowRequested(sessionId))
    try {
      await vi.waitFor(() =>
        expect(selectSessionSync(store.getState())).toMatchObject({
          phase: 'live',
          cursor: '1042',
          drafts: [{ content: 'Text while history stalls' }],
        }),
      )
      expect(readSessionLive).toHaveBeenCalledTimes(1)
      expect(source.readDescriptor).toHaveBeenCalledTimes(1)
      expect(extensions).toHaveBeenCalledTimes(branch === 'snapshot' ? 1 : 2)
      expect(source.readWindow).toHaveBeenCalledTimes(stalledRead === 'load' ? 1 : 0)
      release()
      await vi.waitFor(() =>
        expect(queries.getQueryData<SessionWorkspace>(key)?.descriptor.observed_through).toBe(
          '1042',
        ),
      )
      expect(source.readDescriptor).toHaveBeenCalledTimes(2)
      expect(source.readWindow).toHaveBeenCalledTimes(1)
      expect(extensions).toHaveBeenCalledTimes(branch === 'snapshot' ? 2 : 3)
      expect(extensions.mock.lastCall?.[2]).toBe('1042')
    } finally {
      extensions.mockRestore()
      stop()
      release()
      queries.clear()
    }
  },
)

it.each([
  { invalid: 'inactive turn', active: null },
  {
    invalid: 'missing model call',
    active: { turn_id: draftSessionId, state: { kind: 'running' as const, model_call_id: null } },
  },
  {
    invalid: 'different turn',
    active: {
      turn_id: '00000000-0000-0000-0000-000000000994',
      state: { kind: 'running' as const, model_call_id: draftSessionId },
    },
  },
  {
    invalid: 'different model call',
    active: {
      turn_id: draftSessionId,
      state: { kind: 'running' as const, model_call_id: '00000000-0000-0000-0000-000000000994' },
    },
  },
  {
    invalid: 'call awaiting recovery',
    active: {
      turn_id: draftSessionId,
      state: { kind: 'awaiting_model_call_recovery' as const, model_call_id: draftSessionId },
    },
  },
])('resynchronizes instead of rendering a provider delta for $invalid', async ({ active }) => {
  let requested = false
  vi.mocked(followSession).mockImplementation(async function* (_sessionId, _signal, needsResync) {
    yield { kind: 'snapshot', snapshot: { ...runningSnapshot(), active } }
    yield draft('Uncorrelated text')
    requested = needsResync?.() ?? false
  })
  const store = createAppStore()
  const queries = new QueryClient()
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  try {
    await vi.waitFor(() => expect(requested).toBe(true))
    expect(selectSessionSync(store.getState())).toMatchObject({
      phase: 'resyncing',
      snapshot: null,
      drafts: [],
    })
  } finally {
    stop()
    queries.clear()
  }
})

it('clears published transient state on resync while retaining a newer side-read cursor', async () => {
  vi.mocked(followSession).mockImplementation(async function* () {
    yield { kind: 'snapshot', snapshot: runningSnapshot() }
    yield draft('Retained prefix')
    yield {
      kind: 'durable',
      cursor: '42',
      address: { event_sequence: '42' },
      event_kind: 'input_accepted',
    }
    yield { kind: 'resync_required', cursor: '43' }
  })
  vi.mocked(readSessionLive).mockResolvedValue({ ...runningSnapshot(), observed_through: '50' })
  const store = createAppStore()
  const queries = new QueryClient()
  let sawPublishedDraft = false
  const unsubscribe = store.subscribe(() => {
    const live = selectSessionSync(store.getState())
    if (live.cursor === '50' && live.drafts[0]?.content === 'Retained prefix')
      sawPublishedDraft = true
  })
  const stop = startSessionSynchronization(store, queries)
  store.dispatch(actions.sessionFollowRequested(draftSessionId))
  try {
    await vi.waitFor(() =>
      expect(selectSessionSync(store.getState())).toMatchObject({
        phase: 'resyncing',
        cursor: '50',
        snapshot: null,
        drafts: [],
      }),
    )
    expect(sawPublishedDraft).toBe(true)
  } finally {
    stop()
    unsubscribe()
    queries.clear()
  }
})

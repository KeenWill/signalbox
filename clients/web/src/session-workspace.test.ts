import { QueryClient } from '@tanstack/react-query'
import { expect, it, vi } from 'vitest'
import {
  BoundedSessionHistory,
  type SessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { extendSessionWorkspace, type SessionWorkspace } from './session-workspace'

const sessionId = '00000000-0000-0000-0000-000000000991'
const item = (sequence: number) => ({
  address: { event_sequence: String(sequence) },
  kind: 'input_accepted' as const,
  projected_structured_bytes: 78,
})
const descriptor = (count: number) => ({
  session_id: sessionId,
  observed_through: String(count),
  first_address: { event_sequence: '1' },
  latest_address: { event_sequence: String(count) },
  supervision: null,
  repository_watch: null,
  workspace_root_kind: null,
  sizes: {
    item_count: String(count),
    projected_structured_bytes: String(count * 78),
    projected_text_bytes: '0',
    referenced_blob_count: '0',
    referenced_blob_bytes: '0',
  },
  work: { active_turn_count: '0', queued_turn_count: '0' },
})

it.each([
  {
    anchor: { kind: 'latest' } as SessionWindowAnchor,
    work: descriptor(82).work,
    continuation: null,
  },
  {
    anchor: { kind: 'around', eventSequence: '40' } as SessionWindowAnchor,
    work: { active_turn_count: '1', queued_turn_count: '0' },
    continuation: { event_sequence: '80' },
  },
  {
    anchor: { kind: 'around', eventSequence: '40' } as SessionWindowAnchor,
    work: { active_turn_count: '0', queued_turn_count: '1' },
    continuation: { event_sequence: '80' },
  },
])(
  'extends $anchor.kind toward the bounded tail with work $work',
  async ({ anchor, work, continuation }) => {
    const source: SessionTimelineSource = {
      limits: { max_timeline_window_items: 256, max_timeline_window_bytes: 65_536 },
      readDescriptor: vi.fn().mockResolvedValue({ ...descriptor(82), work }),
      readWindow: vi.fn().mockResolvedValue({
        session_id: sessionId,
        items: [item(81), item(82)],
        projected_structured_bytes: 156,
        continuation_before: { event_sequence: '81' },
        continuation_after: null,
      }),
    }
    const queries = new QueryClient()
    const queryKey = ['production', 'session-workspace', sessionId]
    const held: SessionWorkspace = {
      active: false,
      anchor,
      descriptor: descriptor(80),
      history: new BoundedSessionHistory(sessionId, source),
      window: {
        session_id: sessionId,
        items: Array.from({ length: 80 }, (_, index) => item(index + 1)),
        projected_structured_bytes: 80 * 78,
        continuation_before: null,
        continuation_after: continuation,
      },
    }
    queries.setQueryData(queryKey, held)
    const reload = vi.spyOn(queries, 'invalidateQueries')
    const signal = new AbortController().signal
    await extendSessionWorkspace(queries, sessionId, '82', signal)
    const extended = queries.getQueryData<SessionWorkspace>(queryKey)
    expect(source.readWindow).toHaveBeenCalledExactlyOnceWith(
      sessionId,
      { kind: 'after', eventSequence: '80' },
      { maxItems: 80, maxBytes: 65_536 },
      signal,
    )
    expect(extended?.anchor).toEqual({ kind: 'latest' })
    expect(extended?.window.continuation_after).toBeNull()
    expect(extended?.window.items).toHaveLength(80)
    expect(extended?.window.items[0]).toEqual(item(3))
    expect(extended?.window.items.at(-1)).toEqual(item(82))
    expect(extended?.window.continuation_before).toEqual(item(3).address)
    expect(reload).not.toHaveBeenCalled()
    await extendSessionWorkspace(queries, sessionId, '82', signal)
    expect(source.readDescriptor).toHaveBeenCalledTimes(1)
    queries.clear()
  },
)

it('reloads a bounded window when an extension cannot reach the observed tail', async () => {
  const source: SessionTimelineSource = {
    limits: { max_timeline_window_items: 256, max_timeline_window_bytes: 65_536 },
    readDescriptor: vi.fn().mockResolvedValue(descriptor(1000)),
    readWindow: vi.fn().mockResolvedValue({
      session_id: sessionId,
      items: [item(2)],
      projected_structured_bytes: 78,
      continuation_before: { event_sequence: '2' },
      continuation_after: { event_sequence: '2' },
    }),
  }
  const queries = new QueryClient()
  const queryKey = ['production', 'session-workspace', sessionId]
  queries.setQueryData<SessionWorkspace>(queryKey, {
    active: false,
    anchor: { kind: 'latest' },
    descriptor: descriptor(1),
    history: new BoundedSessionHistory(sessionId, source),
    window: {
      session_id: sessionId,
      items: [item(1)],
      projected_structured_bytes: 78,
      continuation_before: null,
      continuation_after: null,
    },
  })
  const reload = vi.spyOn(queries, 'invalidateQueries')
  await extendSessionWorkspace(queries, sessionId, '1000', new AbortController().signal)
  expect(reload).toHaveBeenCalledExactlyOnceWith({ queryKey, exact: true })
  expect(queries.getQueryData<SessionWorkspace>(queryKey)?.window.items).toEqual([item(1)])
  queries.clear()
})

it.each([
  {
    anchor: { kind: 'first' } as SessionWindowAnchor,
    work: { active_turn_count: '1', queued_turn_count: '0' },
  },
  {
    anchor: { kind: 'around', eventSequence: '40' } as SessionWindowAnchor,
    work: descriptor(81).work,
  },
])(
  'preserves $anchor.kind with work $work and exposes newer history through continuation',
  async ({ anchor, work }) => {
    const source: SessionTimelineSource = {
      limits: { max_timeline_window_items: 256, max_timeline_window_bytes: 65_536 },
      readDescriptor: vi.fn().mockResolvedValue({ ...descriptor(81), work }),
      readWindow: vi.fn(),
    }
    const queries = new QueryClient()
    const queryKey = ['production', 'session-workspace', sessionId]
    queries.setQueryData<SessionWorkspace>(queryKey, {
      active: false,
      anchor,
      descriptor: descriptor(80),
      history: new BoundedSessionHistory(sessionId, source),
      window: {
        session_id: sessionId,
        items: Array.from({ length: 80 }, (_, index) => item(index + 1)),
        projected_structured_bytes: 80 * 78,
        continuation_before: null,
        continuation_after: null,
      },
    })
    const reload = vi.spyOn(queries, 'invalidateQueries')
    await extendSessionWorkspace(queries, sessionId, '81', new AbortController().signal)
    const held = queries.getQueryData<SessionWorkspace>(queryKey)
    expect(held?.anchor).toEqual(anchor)
    expect(held?.window.items).toHaveLength(80)
    expect(held?.window.items[0]).toEqual(item(1))
    expect(held?.window.items.at(-1)).toEqual(item(80))
    expect(held?.window.continuation_after).toEqual(item(80).address)
    expect(source.readWindow).not.toHaveBeenCalled()
    expect(reload).not.toHaveBeenCalled()
    queries.clear()
  },
)

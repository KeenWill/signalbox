import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineWindow } from './generated/web-contract.mjs'
import {
  activityLabel,
  boundarySessionItemId,
  conflictingDurableKind,
  pruneExpandedSessionItems,
  reconcileVisibleSessionSelection,
  sessionHasLiveWork,
  sessionWorkspaceQueryKey,
  visibleSessionItems,
} from './SessionWorkspaceSurface'

const fixture = decodeWebSessionTimelineWindow({
  session_id: '00000000-0000-0000-0000-000000000991',
  items: [
    {
      address: { event_sequence: '41' },
      kind: 'input_accepted',
      projected_structured_bytes: 96,
    },
    {
      address: { event_sequence: '42' },
      kind: 'turn_completed',
      projected_structured_bytes: 96,
    },
    {
      address: { event_sequence: '43' },
      kind: 'turn_failed',
      projected_structured_bytes: 96,
    },
    {
      address: { event_sequence: '44' },
      kind: 'turn_reconciliation_required',
      projected_structured_bytes: 96,
    },
  ],
  projected_structured_bytes: 384,
  continuation_before: { event_sequence: '41' },
  continuation_after: null,
})

describe('Session Workspace projection', () => {
  it('keeps full and condensed modes over the same bounded window', () => {
    expect(visibleSessionItems(fixture.items, 'full')).toBe(fixture.items)
    expect(visibleSessionItems(fixture.items, 'condensed')).toBe(fixture.items)
  })

  it('projects result mode without materializing another window', () => {
    const results = visibleSessionItems(fixture.items, 'results')

    expect(results).toEqual([
      fixture.items[0],
      fixture.items[1],
      fixture.items[2],
      fixture.items[3],
    ])
  })

  it('uses one stable cache entry for every request for a session', () => {
    expect(sessionWorkspaceQueryKey(fixture.session_id)).toEqual([
      'production',
      'session-workspace',
      fixture.session_id,
    ])
  })

  it('selects the visible row at the successfully loaded boundary', () => {
    expect(boundarySessionItemId(fixture.items, 'full', 'first')).toBe('41')
    expect(boundarySessionItemId(fixture.items, 'full', 'latest')).toBe('44')
    expect(boundarySessionItemId([], 'results', 'latest')).toBeNull()
  })

  it('reconciles selection to a stable visible option', () => {
    expect(reconcileVisibleSessionSelection(null, ['41', '42'])).toBe('41')
    expect(reconcileVisibleSessionSelection('42', ['41', '42'])).toBe('42')
    expect(reconcileVisibleSessionSelection('44', ['41', '42'])).toBe('41')
    expect(reconcileVisibleSessionSelection('44', [])).toBeNull()
  })

  it('treats active or queued turns as live work', () => {
    expect(sessionHasLiveWork('1', '0')).toBe(true)
    expect(sessionHasLiveWork('0', '2')).toBe(true)
    expect(sessionHasLiveWork('0', '0')).toBe(false)
  })

  it('retains expansion state only for the current bounded window', () => {
    const expanded = new Set(['40', '41', '44', '45'])

    expect([...pruneExpandedSessionItems(expanded, fixture.items)]).toEqual(['41', '44'])
  })

  it('falls back to the raw value outside the JavaScript Date range', () => {
    expect(activityLabel('9000000000000000000')).toBe('9000000000000000000')
  })

  it('reports a durable header whose kind contradicts the loaded historical window', () => {
    const contradicting = {
      address: { event_sequence: '42' },
      cursor: '42',
      event_kind: 'turn_failed' as const,
      kind: 'durable' as const,
    }

    expect(conflictingDurableKind(fixture.items, [contradicting])).toEqual({
      eventSequence: '42',
      historicalKind: fixture.items[1]?.kind,
      liveKind: 'turn_failed',
    })
  })

  it('accepts durable headers that agree with or extend the loaded historical window', () => {
    const agreeing = {
      address: { event_sequence: '42' },
      cursor: '42',
      event_kind: 'turn_completed' as const,
      kind: 'durable' as const,
    }
    const beyondWindow = {
      address: { event_sequence: '45' },
      cursor: '45',
      event_kind: 'turn_failed' as const,
      kind: 'durable' as const,
    }

    expect(conflictingDurableKind(fixture.items, [agreeing, beyondWindow])).toBe(null)
  })
})

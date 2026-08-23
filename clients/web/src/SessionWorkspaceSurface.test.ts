import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineWindow } from './generated/web-contract.mjs'
import {
  isCanonicalSessionId,
  reconcileTimelineSelection,
  retainSameSessionPlaceholder,
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
  ],
  projected_structured_bytes: 288,
  continuation_before: { event_sequence: '41' },
  continuation_after: null,
})

describe('Session Workspace projection', () => {
  it('accepts only canonical session identities', () => {
    expect(isCanonicalSessionId(fixture.session_id)).toBe(true)
    expect(isCanonicalSessionId(fixture.session_id.replaceAll('-', ''))).toBe(false)
    expect(isCanonicalSessionId('not-a-session')).toBe(false)
  })

  it('keeps full and condensed modes over the same bounded window', () => {
    expect(visibleSessionItems(fixture.items, 'full')).toBe(fixture.items)
    expect(visibleSessionItems(fixture.items, 'condensed')).toBe(fixture.items)
  })

  it('projects result mode without materializing another window', () => {
    const results = visibleSessionItems(fixture.items, 'results')

    expect(results).toEqual(fixture.items)
  })

  it('reconciles a hidden selection to the first visible timeline row', () => {
    expect(reconcileTimelineSelection('42', ['41', '43'])).toBe('41')
    expect(reconcileTimelineSelection('43', ['41', '43'])).toBe('43')
    expect(reconcileTimelineSelection('42', [])).toBeNull()
  })

  it('retains placeholder data only while navigating within the same session', () => {
    const previous = { sessionId: fixture.session_id, window: fixture }

    expect(retainSameSessionPlaceholder(previous, fixture.session_id)).toBe(previous)
    expect(
      retainSameSessionPlaceholder(previous, '00000000-0000-0000-0000-000000000992'),
    ).toBeUndefined()
  })
})

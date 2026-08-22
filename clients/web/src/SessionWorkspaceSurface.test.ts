import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineWindow } from './generated/web-contract.mjs'
import { isCompatibleDetailBody } from './SessionItemDetail'
import {
  isCanonicalSessionId,
  timelineArrowTarget,
  visibleSessionItems,
} from './SessionWorkspaceSurface'

const fixture = decodeWebSessionTimelineWindow({
  session_id: '00000000-0000-0000-0000-000000000991',
  items: [
    {
      address: { event_sequence: '41' },
      kind: 'input_accepted',
      projected_structured_bytes: 78,
    },
    {
      address: { event_sequence: '42' },
      kind: 'turn_completed',
      projected_structured_bytes: 78,
    },
    {
      address: { event_sequence: '43' },
      kind: 'turn_failed',
      projected_structured_bytes: 75,
    },
    {
      address: { event_sequence: '44' },
      kind: 'turn_activated',
      projected_structured_bytes: 78,
    },
    {
      address: { event_sequence: '45' },
      kind: 'model_call_transition',
      projected_structured_bytes: 85,
    },
  ],
  projected_structured_bytes: 394,
  continuation_before: { event_sequence: '41' },
  continuation_after: null,
})

describe('Session Workspace projection', () => {
  it('accepts only canonical session identities', () => {
    expect(isCanonicalSessionId(fixture.session_id)).toBe(true)
    expect(isCanonicalSessionId(fixture.session_id.replaceAll('-', ''))).toBe(false)
    expect(isCanonicalSessionId('not-a-session')).toBe(false)
  })

  it('uses a distinct condensed projection over the same bounded window', () => {
    expect(visibleSessionItems(fixture.items, 'full')).toBe(fixture.items)
    expect(visibleSessionItems(fixture.items, 'condensed')).toEqual([
      fixture.items[0],
      fixture.items[1],
      fixture.items[2],
      fixture.items[4],
    ])
  })

  it('projects result mode without materializing another window', () => {
    const results = visibleSessionItems(fixture.items, 'results')

    expect(results).toEqual([
      fixture.items[0],
      fixture.items[1],
      fixture.items[2],
      fixture.items[4],
    ])
  })

  it('moves focused timeline selection with arrow keys', () => {
    const ids = ['41', '42', '43']

    expect(timelineArrowTarget(ids, '41', 'ArrowDown')).toBe('42')
    expect(timelineArrowTarget(ids, '42', 'ArrowUp')).toBe('41')
    expect(timelineArrowTarget(ids, null, 'ArrowDown')).toBe('41')
    expect(timelineArrowTarget(ids, '42', 'Enter')).toBeUndefined()
  })

  it('rejects detail bodies that do not belong to the advertised event kind', () => {
    const lifecycleBody = {
      type: 'turn_lifecycle',
      turn_id: '00000000-0000-0000-0000-000000000041',
      lifecycle: 'terminalized',
      cause_code: 'completed',
    } as const

    expect(isCompatibleDetailBody('input_accepted', lifecycleBody)).toBe(false)
    expect(isCompatibleDetailBody('turn_completed', lifecycleBody)).toBe(true)
    expect(isCompatibleDetailBody('turn_failed', lifecycleBody)).toBe(false)
    expect(
      isCompatibleDetailBody('turn_activated', {
        ...lifecycleBody,
        lifecycle: 'activated',
        cause_code: 'activated',
      }),
    ).toBe(true)
    expect(isCompatibleDetailBody('turn_failed', { ...lifecycleBody, cause_code: 'failed' })).toBe(
      true,
    )
    expect(
      isCompatibleDetailBody('turn_refused', { ...lifecycleBody, cause_code: 'refused' }),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('turn_cancelled', { ...lifecycleBody, cause_code: 'cancelled' }),
    ).toBe(true)
  })
})

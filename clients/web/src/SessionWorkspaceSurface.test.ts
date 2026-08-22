import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineWindow } from './generated/web-contract.mjs'
import { isCompatibleDetailBody } from './SessionItemDetail'
import { isCanonicalSessionId, visibleSessionItems } from './SessionWorkspaceSurface'

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
      kind: 'turn_activated',
      projected_structured_bytes: 96,
    },
  ],
  projected_structured_bytes: 384,
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
    expect(visibleSessionItems(fixture.items, 'condensed')).toEqual(fixture.items.slice(0, 3))
  })

  it('projects result mode without materializing another window', () => {
    const results = visibleSessionItems(fixture.items, 'results')

    expect(results).toEqual([fixture.items[0], fixture.items[1], fixture.items[2]])
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
  })
})

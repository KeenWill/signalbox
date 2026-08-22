import { describe, expect, it } from 'vitest'
import {
  decodeWebContractBootstrap,
  decodeWebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import { isCompatibleDetailBody } from './SessionItemDetail'
import {
  hasUsableSessionTimeline,
  isCanonicalSessionId,
  restoredTimelineSelection,
  sameSessionWindowAnchor,
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

  it('restores a saved selection only when its projected row is visible', () => {
    expect(restoredTimelineSelection('42', true, ['41', '42'])).toBe('42')
    expect(restoredTimelineSelection('42', true, ['41'])).toBeUndefined()
    expect(restoredTimelineSelection('42', false, ['42'])).toBeUndefined()
  })

  it('identifies repeated window anchors without allocating query attempts', () => {
    expect(sameSessionWindowAnchor({ kind: 'latest' }, { kind: 'latest' })).toBe(true)
    expect(sameSessionWindowAnchor({ kind: 'first' }, { kind: 'latest' })).toBe(false)
    expect(
      sameSessionWindowAnchor(
        { kind: 'after', eventSequence: '42' },
        { kind: 'after', eventSequence: '42' },
      ),
    ).toBe(true)
    expect(
      sameSessionWindowAnchor(
        { kind: 'after', eventSequence: '42' },
        { kind: 'after', eventSequence: '43' },
      ),
    ).toBe(false)
  })

  it('moves focused timeline selection with arrow keys', () => {
    const ids = ['41', '42', '43']

    expect(timelineArrowTarget(ids, '41', 'ArrowDown')).toBe('42')
    expect(timelineArrowTarget(ids, '42', 'ArrowUp')).toBe('41')
    expect(timelineArrowTarget(ids, null, 'ArrowDown')).toBe('41')
    expect(timelineArrowTarget(ids, '42', 'Enter')).toBeUndefined()
  })

  it('rejects readiness when advertised window limits exceed client ceilings', () => {
    const bootstrap = decodeWebContractBootstrap({
      contract: { name: 'signalbox.web-http', version: '1' },
      capabilities: {
        bounded_json: true,
        same_origin_json_mutations: true,
        ndjson_streaming: true,
        bounded_session_timeline: true,
        bounded_session_timeline_detail: true,
      },
      limits: {
        max_json_body_bytes: 1024,
        max_ndjson_item_bytes: 1024,
        max_timeline_window_items: 256,
        max_timeline_window_bytes: 64 * 1024,
        max_timeline_detail_items: 128,
        max_timeline_detail_bytes: 64 * 1024,
      },
    })

    expect(hasUsableSessionTimeline(bootstrap)).toBe(true)
    expect(
      hasUsableSessionTimeline({
        ...bootstrap,
        limits: { ...bootstrap.limits, max_timeline_window_items: 257 },
      }),
    ).toBe(false)
    expect(
      hasUsableSessionTimeline({
        ...bootstrap,
        limits: { ...bootstrap.limits, max_timeline_window_bytes: 64 * 1024 + 1 },
      }),
    ).toBe(false)
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

    const sessionSettings = {
      type: 'model_settings',
      turn_id: null,
      cause_code: 'session_defaults_changed',
    } as const
    expect(isCompatibleDetailBody('session_model_settings_changed', sessionSettings)).toBe(true)
    expect(
      isCompatibleDetailBody('session_model_settings_changed', {
        ...sessionSettings,
        turn_id: '00000000-0000-0000-0000-000000000041',
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('turn_model_settings_resolved', {
        ...sessionSettings,
        turn_id: '00000000-0000-0000-0000-000000000041',
        cause_code: 'turn_settings_resolved',
      }),
    ).toBe(true)
    expect(isCompatibleDetailBody('turn_model_settings_resolved', sessionSettings)).toBe(false)
  })
})

import { describe, expect, it } from 'vitest'
import {
  decodeWebContractBootstrap,
  decodeWebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import { isCompatibleDetailBody } from './SessionItemDetail'
import {
  hasUsableSessionTimeline,
  isCanonicalSessionId,
  projectedTimelineSelection,
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

  it('re-homes a selection removed by detail projection', () => {
    expect(projectedTimelineSelection('44', ['41', '42', '45'])).toBe('41')
    expect(projectedTimelineSelection('42', ['41', '42', '45'])).toBe('42')
    expect(projectedTimelineSelection('44', [])).toBeNull()
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
      detail: { type: 'session_defaults_changed' },
    } as const
    expect(
      isCompatibleDetailBody(
        'session_model_settings_changed',
        sessionSettings as unknown as Parameters<typeof isCompatibleDetailBody>[1],
      ),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('session_model_settings_changed', {
        ...sessionSettings,
        detail: { type: 'turn_resolved' },
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('turn_model_settings_resolved', {
        ...sessionSettings,
        detail: { type: 'turn_resolved' },
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(true)
    expect(
      isCompatibleDetailBody(
        'turn_model_settings_resolved',
        sessionSettings as unknown as Parameters<typeof isCompatibleDetailBody>[1],
      ),
    ).toBe(false)

    const delegation = {
      type: 'delegation',
      detail: {
        type: 'child_spawned',
        relationship_id: '00000000-0000-0000-0000-000000000041',
        child_session_id: '00000000-0000-0000-0000-000000000042',
        policy: { type: 'background' },
      },
    } as const
    expect(isCompatibleDetailBody('delegation_update', delegation)).toBe(true)
    expect(
      isCompatibleDetailBody('delegation_update', {
        ...delegation,
        detail: {
          type: 'result_wake',
          relationship_id: delegation.detail.relationship_id,
        },
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('delegation_wake', {
        ...delegation,
        detail: {
          type: 'message_wake',
          relationship_id: delegation.detail.relationship_id,
          message_id: '00000000-0000-0000-0000-000000000043',
        },
      }),
    ).toBe(true)

    const goalEvent = {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000041',
      event: {
        type: 'blocked',
        generation: '1',
        reason: 'authorization_required',
        text: { text: '', offset_bytes: '0', total_bytes: '0' },
      },
    } as const
    expect(isCompatibleDetailBody('goal_turn_retired', goalEvent)).toBe(true)
    expect(
      isCompatibleDetailBody('goal_turn_retired', {
        ...goalEvent,
        event: { ...goalEvent.event, reason: 'invented' },
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('goal_turn_retired', {
        ...goalEvent,
        event: { type: 'achieved', generation: '1', text: goalEvent.event.text },
      }),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('goal_turn_retired', {
        ...goalEvent,
        event: { ...goalEvent.event, type: 'invented' },
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(false)

    const toolBatch = {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000041',
      producing_model_call_id: '00000000-0000-0000-0000-000000000042',
      state: {
        type: 'results_projected',
        frontier_id: '00000000-0000-0000-0000-000000000045',
      },
      tools: [],
      goal_events: [goalEvent.event],
    } as const
    expect(isCompatibleDetailBody('tool_batch_transition', toolBatch)).toBe(true)
    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        goal_events: [{ ...goalEvent.event, reason: 'invented' }],
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000043',
            tool_name: 'workspace_read',
            approval_posture: 'human',
            approval_judge_escalated: false,
            operator_required: false,
          },
        ],
      }),
    ).toBe(false)

    const modelCall = {
      type: 'model_call',
      turn_id: '00000000-0000-0000-0000-000000000041',
      model_call_id: '00000000-0000-0000-0000-000000000042',
      model_identity_id: '00000000-0000-0000-0000-000000000043',
      request_context_items: '1',
      response: null,
      usage: {},
      state: { type: 'terminal', disposition: 'known_failed' },
      cause_code: 'quota_exhausted',
    } as const
    expect(isCompatibleDetailBody('model_call_transition', modelCall)).toBe(true)
    expect(
      isCompatibleDetailBody('model_call_transition', { ...modelCall, cause_code: 'invented' }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('model_call_transition', {
        ...modelCall,
        state: { type: 'in_flight' },
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('model_call_transition', {
        ...modelCall,
        state: { type: 'in_flight' },
        cause_code: null,
        usage: { input_tokens: '1' },
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('model_call_transition', {
        ...modelCall,
        state: { type: 'in_flight' },
        cause_code: null,
        usage: {},
      }),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('model_call_transition', {
        ...modelCall,
        state: { type: 'in_flight' },
        cause_code: null,
        usage: {},
        response: { text: 'premature', offset_bytes: '0', total_bytes: '9' },
      }),
    ).toBe(false)

    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000043',
            tool_name: 'workspace_read',
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: false,
            arguments: null,
            attempt_id: null,
            state: 'completed',
            effect_posture: null,
            sandbox_posture: null,
            result: null,
            failure: null,
            cause_code: null,
          },
        ],
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000043',
            tool_name: 'workspace_read',
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: false,
            arguments: null,
            attempt_id: '00000000-0000-0000-0000-000000000044',
            state: null,
            effect_posture: 'effect_free',
            sandbox_posture: null,
            result: null,
            failure: null,
            cause_code: null,
          },
        ],
      }),
    ).toBe(false)

    const issuedTool = {
      request_id: '00000000-0000-0000-0000-000000000043',
      tool_name: 'workspace_read',
      approval_posture: 'auto',
      approval_judge_escalated: false,
      operator_required: false,
      arguments: null,
      attempt_id: '00000000-0000-0000-0000-000000000044',
      state: 'known_failed',
      effect_posture: 'effect_free',
      sandbox_posture: 'sandboxed',
      result: null,
      failure: { text: 'failed', offset_bytes: '0', total_bytes: '6' },
      cause_code: 'execution_failed',
    } as const
    expect(
      isCompatibleDetailBody('tool_batch_transition', { ...toolBatch, tools: [issuedTool] }),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        tools: [{ ...issuedTool, state: 'completed' }],
      }),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('tool_batch_transition', {
        ...toolBatch,
        tools: [{ ...issuedTool, cause_code: 'invented' }],
      }),
    ).toBe(false)

    const reconciliation = {
      type: 'reconciliation',
      turn_id: '00000000-0000-0000-0000-000000000041',
      operation: {
        type: 'model_call',
        model_call_id: '00000000-0000-0000-0000-000000000042',
      },
      attempt_count: '2',
      cause_code: 'ambiguous_operation',
      exhausted: true,
      operator_required: true,
    } as const
    expect(isCompatibleDetailBody('turn_reconciliation_required', reconciliation)).toBe(true)
    expect(
      isCompatibleDetailBody('turn_reconciliation_required', {
        ...reconciliation,
        operation: { type: 'invented' },
      } as unknown as typeof reconciliation),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('turn_reconciliation_required', {
        ...reconciliation,
        exhausted: false,
      }),
    ).toBe(false)

    const approval = {
      type: 'tool_approval_decision',
      turn_id: '00000000-0000-0000-0000-000000000041',
      request_id: '00000000-0000-0000-0000-000000000042',
      tool_name: 'workspace_read',
      decision: 'approve',
      actor: { type: 'user', command_id: '00000000-0000-0000-0000-000000000043' },
      rationale: null,
      approval_judge_escalated: false,
    } as const
    expect(isCompatibleDetailBody('tool_approval_decided', approval)).toBe(true)
    expect(
      isCompatibleDetailBody('tool_approval_decided', {
        ...approval,
        actor: { type: 'invented' },
      } as unknown as Parameters<typeof isCompatibleDetailBody>[1]),
    ).toBe(false)
    expect(
      isCompatibleDetailBody('tool_approval_decided', {
        ...approval,
        actor: { type: 'policy' },
      }),
    ).toBe(true)
    expect(
      isCompatibleDetailBody('tool_approval_decided', {
        ...approval,
        decision: 'defer',
      } as unknown as typeof approval),
    ).toBe(false)
  })
})

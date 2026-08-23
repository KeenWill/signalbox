import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineDetailPage } from './generated/web-contract.mjs'

const ambiguousModelCallItem = {
  address: { event_sequence: '7' },
  kind: 'model_call_transition',
  body: {
    type: 'model_call',
    turn_id: '00000000-0000-0000-0000-000000000002',
    model_call_id: '00000000-0000-0000-0000-000000000003',
    state: { type: 'terminal', disposition: 'ambiguous' },
    model_identity_id: 'anthropic:claude-sonnet',
    request_context_items: '4',
    usage: {},
    cause_code: 'transport_interrupted',
  },
  projected_body_bytes: 128,
}

const ambiguousModelCallPage = {
  session_id: '00000000-0000-0000-0000-000000000001',
  items: [ambiguousModelCallItem],
  projected_body_bytes: 128,
}

function expectTimelineBodyRejected(kind: string, body: object) {
  const page = {
    session_id: ambiguousModelCallPage.session_id,
    items: [
      {
        address: { event_sequence: '13' },
        kind,
        body,
        projected_body_bytes: 128,
      },
    ],
    projected_body_bytes: 128,
  }

  expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
    'timeline_detail_page.items[0].body must be one recognized variant',
  )
}

describe('generated timeline detail decoder', () => {
  it('accepts an ambiguous terminal model call with its disposition in-band', () => {
    const page = decodeWebSessionTimelineDetailPage(ambiguousModelCallPage)

    expect(page.items[0]?.body).toEqual(ambiguousModelCallPage.items[0]?.body)
  })

  it('rejects a terminal model call without a disposition', () => {
    const invalidPage = {
      ...ambiguousModelCallPage,
      items: [
        {
          ...ambiguousModelCallItem,
          body: { ...ambiguousModelCallItem.body, state: { type: 'terminal' } },
        },
      ],
    }

    expect(() => decodeWebSessionTimelineDetailPage(invalidPage)).toThrow(
      'timeline_detail_page.items[0].body must be one recognized variant',
    )
  })

  it('accepts a progressively bounded denied tool attempt with judge evidence', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '9' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            state: {
              type: 'results_projected',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000004',
                tool_name: 'exec',
                arguments: {
                  text: '{"cmd":"cargo test"}',
                  offset_bytes: '0',
                  total_bytes: '20',
                },
                approval_posture: 'delegated',
                approval_judge_escalated: true,
                operator_required: true,
                state: 'known_failed',
                cause_code: 'denied',
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 148,
        },
      ],
      projected_body_bytes: 148,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('accepts exact delegate approval provenance', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '10' },
          kind: 'tool_approval_decided',
          body: {
            type: 'tool_approval_decision',
            turn_id: '00000000-0000-0000-0000-000000000002',
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            decision: 'approve',
            actor: {
              type: 'delegate',
              model_selection_id: '00000000-0000-0000-0000-000000000005',
              model_call_id: '00000000-0000-0000-0000-000000000006',
            },
            approval_judge_escalated: true,
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects an unknown tool-batch state', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '9' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            state: 'future_state',
            tools: [],
            goal_events: [],
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'timeline_detail_page.items[0].body must be one recognized variant',
    )
  })

  it('rejects unknown runner sandbox posture', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '11' },
          kind: 'runner_state_transition',
          body: {
            type: 'runner',
            runner_id: '00000000-0000-0000-0000-000000000007',
            placement_revision: '1',
            sandbox_posture: 'future_sandbox',
            state: 'pinned',
          },
          projected_body_bytes: 96,
        },
      ],
      projected_body_bytes: 96,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'timeline_detail_page.items[0].body must be one recognized variant',
    )
  })

  it('rejects unknown runner state', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '12' },
          kind: 'runner_state_transition',
          body: {
            type: 'runner',
            runner_id: '00000000-0000-0000-0000-000000000007',
            placement_revision: '1',
            sandbox_posture: 'sandboxed',
            state: 'future_state',
          },
          projected_body_bytes: 96,
        },
      ],
      projected_body_bytes: 96,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'timeline_detail_page.items[0].body must be one recognized variant',
    )
  })

  it('accepts awaiting-child tool attempts', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '13' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            state: {
              type: 'results_projected',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000004',
                tool_name: 'spawn_session',
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                state: 'awaiting_child',
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('accepts closed goal values and rejects fabricated values', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '14' },
          kind: 'goal_turn_retired',
          body: {
            type: 'goal_event',
            turn_id: '00000000-0000-0000-0000-000000000002',
            event: {
              type: 'blocked',
              generation: '7',
              reason: 'authorization_required',
              text: { text: 'approval needed', offset_bytes: '0', total_bytes: '15' },
            },
          },
          projected_body_bytes: 143,
        },
      ],
      projected_body_bytes: 143,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { generation: '7', event_kind: 'retired' },
    })
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { generation: '7', event_kind: 'blocked', reason: 'future_reason' },
    })
  })

  it('accepts background and bound delegation policies', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '15' },
          kind: 'delegation_update',
          body: {
            type: 'delegation',
            detail: {
              type: 'child_spawned',
              relationship_id: '00000000-0000-0000-0000-000000000004',
              child_session_id: '00000000-0000-0000-0000-000000000005',
              policy: { type: 'background' },
            },
          },
          projected_body_bytes: 128,
        },
        {
          address: { event_sequence: '16' },
          kind: 'delegation_update',
          body: {
            type: 'delegation',
            detail: {
              type: 'child_spawned',
              relationship_id: '00000000-0000-0000-0000-000000000006',
              child_session_id: '00000000-0000-0000-0000-000000000007',
              policy: {
                type: 'bound',
                on_parent_stopped: 'stop',
                on_parent_cancelled: 'cancel',
              },
            },
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 256,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects an unknown tool approval posture', () => {
    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000002',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      state: {
        type: 'proposed',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [
        {
          request_id: '00000000-0000-0000-0000-000000000004',
          tool_name: 'exec',
          approval_posture: 'future',
          approval_judge_escalated: false,
          operator_required: false,
        },
      ],
      goal_events: [],
    })
  })

  it('rejects an unknown tool effect posture', () => {
    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000002',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      state: {
        type: 'results_projected',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [
        {
          request_id: '00000000-0000-0000-0000-000000000004',
          tool_name: 'exec',
          approval_posture: 'auto',
          approval_judge_escalated: false,
          operator_required: false,
          effect_posture: 'future',
        },
      ],
      goal_events: [],
    })
  })

  it('rejects an unknown tool sandbox posture', () => {
    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000002',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      state: {
        type: 'results_projected',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [
        {
          request_id: '00000000-0000-0000-0000-000000000004',
          tool_name: 'exec',
          approval_posture: 'auto',
          approval_judge_escalated: false,
          operator_required: false,
          sandbox_posture: 'future_sandbox',
        },
      ],
      goal_events: [],
    })
  })

  it('accepts closed reconciliation operations', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '19' },
          kind: 'turn_reconciliation_required',
          body: {
            type: 'reconciliation',
            turn_id: '00000000-0000-0000-0000-000000000002',
            operation: {
              type: 'model_call',
              model_call_id: '00000000-0000-0000-0000-000000000003',
            },
            attempt_count: '2',
            exhausted: true,
            operator_required: true,
            cause_code: 'ambiguous_operation',
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects an unknown reconciliation operation', () => {
    expectTimelineBodyRejected('turn_reconciliation_required', {
      type: 'reconciliation',
      turn_id: '00000000-0000-0000-0000-000000000002',
      operation: {
        type: 'future',
        operation_id: '00000000-0000-0000-0000-00000000000c',
      },
      attempt_count: '2',
      exhausted: true,
      operator_required: true,
      cause_code: 'ambiguous_operation',
    })
  })

  it('accepts a continued tool argument with exact byte accounting', () => {
    const continuation = {
      address: { event_sequence: '20' },
      field: 'tool_arguments',
      member_index: 0,
      offset_bytes: '4',
    }
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: continuation.address,
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            state: {
              type: 'proposed',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000004',
                tool_name: 'exec',
                arguments: {
                  text: 'abcd',
                  offset_bytes: '0',
                  total_bytes: '8',
                  continuation,
                },
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 132,
        },
      ],
      projected_body_bytes: 132,
      continuation: { type: 'more_body', body: continuation },
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects an unknown approval decision', () => {
    expectTimelineBodyRejected('tool_approval_decided', {
      type: 'tool_approval_decision',
      turn_id: '00000000-0000-0000-0000-000000000002',
      request_id: '00000000-0000-0000-0000-000000000004',
      tool_name: 'exec',
      decision: 'defer',
      actor: {
        type: 'user',
        command_id: '00000000-0000-0000-0000-000000000005',
      },
      approval_judge_escalated: false,
    })
  })

  it('rejects more than one projected tool member', () => {
    const firstTool = {
      request_id: '00000000-0000-0000-0000-000000000004',
      tool_name: 'exec',
      approval_posture: 'auto',
      approval_judge_escalated: false,
      operator_required: false,
    }
    const secondTool = {
      request_id: '00000000-0000-0000-0000-000000000005',
      tool_name: 'read_file',
      approval_posture: 'auto',
      approval_judge_escalated: false,
      operator_required: false,
    }

    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000002',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      state: {
        type: 'proposed',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [firstTool, secondTool],
      goal_events: [],
    })
  })

  it('rejects more than one projected goal member', () => {
    const firstGoal = { generation: '1', event_kind: 'commissioned' }
    const secondGoal = { generation: '2', event_kind: 'achieved' }

    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: '00000000-0000-0000-0000-000000000002',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      state: {
        type: 'proposed',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [],
      goal_events: [firstGoal, secondGoal],
    })
  })

  it('accepts delegation wait mode and lifecycle provenance', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '17' },
          kind: 'delegation_update',
          body: {
            type: 'delegation',
            detail: {
              type: 'child_waiting',
              relationship_id: '00000000-0000-0000-0000-000000000004',
              child_session_id: '00000000-0000-0000-0000-000000000005',
              awaiting_request_id: '00000000-0000-0000-0000-000000000006',
              mode: 'foreground',
            },
          },
          projected_body_bytes: 128,
        },
        {
          address: { event_sequence: '18' },
          kind: 'delegation_update',
          body: {
            type: 'delegation',
            detail: {
              type: 'child_lifecycle_disposition',
              relationship_id: '00000000-0000-0000-0000-000000000004',
              child_session_id: '00000000-0000-0000-0000-000000000005',
              event_ordinal: '2',
              outcome: 'child_stopped',
              reason: 'parent_stopped_with_descendants',
              provenance: {
                type: 'parent_turn_command',
                session_id: '00000000-0000-0000-0000-000000000001',
                turn_id: '00000000-0000-0000-0000-000000000002',
                command_id: '00000000-0000-0000-0000-000000000007',
              },
            },
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 256,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a fabricated delegation variant', () => {
    expectTimelineBodyRejected('delegation_update', {
      type: 'delegation',
      detail: {
        type: 'future',
        relationship_id: '00000000-0000-0000-0000-000000000004',
      },
    })
  })

  it('rejects malformed and out-of-range goal generations', () => {
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { generation: '+1', event_kind: 'superseded' },
    })
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { generation: '01', event_kind: 'superseded' },
    })
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { generation: '18446744073709551616', event_kind: 'superseded' },
    })
  })

  it('rejects malformed and out-of-range imported positions', () => {
    expectTimelineBodyRejected('session_created', {
      type: 'session_created',
      imported_evidence: {
        imported_entry_id: '00000000-0000-0000-0000-000000000008',
        imported_position: '+1',
      },
    })
    expectTimelineBodyRejected('session_created', {
      type: 'session_created',
      imported_evidence: {
        imported_entry_id: '00000000-0000-0000-0000-000000000008',
        imported_position: '01',
      },
    })
    expectTimelineBodyRejected('session_created', {
      type: 'session_created',
      imported_evidence: {
        imported_entry_id: '00000000-0000-0000-0000-000000000008',
        imported_position: '18446744073709551616',
      },
    })
  })

  it('rejects malformed and out-of-range compaction positions', () => {
    const body = {
      type: 'context_compaction',
      compaction_id: '00000000-0000-0000-0000-000000000009',
      model_call_id: '00000000-0000-0000-0000-000000000003',
      summary_entry_id: '00000000-0000-0000-0000-00000000000a',
      result_frontier_id: '00000000-0000-0000-0000-00000000000b',
      summary: { text: 'summary', offset_bytes: '0', total_bytes: '7' },
    }

    expectTimelineBodyRejected('context_compacted', { ...body, through_position: '+1' })
    expectTimelineBodyRejected('context_compacted', { ...body, through_position: '01' })
    expectTimelineBodyRejected('context_compacted', {
      ...body,
      through_position: '18446744073709551616',
    })
  })

  it('rejects malformed and out-of-range reconciliation counts', () => {
    const body = {
      type: 'reconciliation',
      turn_id: '00000000-0000-0000-0000-000000000002',
      operation: {
        type: 'tool_attempt',
        tool_attempt_id: '00000000-0000-0000-0000-00000000000c',
      },
      exhausted: true,
      operator_required: true,
      cause_code: 'ambiguous_operation',
    }

    expectTimelineBodyRejected('turn_reconciliation_required', { ...body, attempt_count: '+1' })
    expectTimelineBodyRejected('turn_reconciliation_required', { ...body, attempt_count: '01' })
    expectTimelineBodyRejected('turn_reconciliation_required', {
      ...body,
      attempt_count: '18446744073709551616',
    })
  })

  it('rejects malformed and out-of-range runner placement revisions', () => {
    const body = {
      type: 'runner',
      runner_id: '00000000-0000-0000-0000-000000000007',
      sandbox_posture: 'sandboxed',
      state: 'pinned',
    }

    expectTimelineBodyRejected('runner_state_transition', { ...body, placement_revision: '+1' })
    expectTimelineBodyRejected('runner_state_transition', { ...body, placement_revision: '01' })
    expectTimelineBodyRejected('runner_state_transition', {
      ...body,
      placement_revision: '18446744073709551616',
    })
  })
})

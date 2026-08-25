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
    model_identity_id: '00000000-0000-0000-0000-000000000004',
    request_context_items: '4',
    response: null,
    usage: {
      input_tokens: null,
      output_tokens: null,
      cache_creation_input_tokens: null,
      cache_read_input_tokens: null,
    },
    provider_failure_cause: null,
  },
  projected_body_bytes: 128,
}

const ambiguousModelCallPage = {
  session_id: '00000000-0000-0000-0000-000000000001',
  items: [ambiguousModelCallItem],
  projected_body_bytes: 128,
}

const inheritedSettingsOverlay = {
  reasoning_level: { kind: 'inherit' },
  fast_mode: { kind: 'inherit' },
  service_tier: { kind: 'inherit' },
}

const settingsSnapshot = {
  precedence: {
    per_call: inheritedSettingsOverlay,
    session: inheritedSettingsOverlay,
    profile: inheritedSettingsOverlay,
    global_default: inheritedSettingsOverlay,
  },
  effective: { fast_mode: 'disabled' },
}

const sessionDefaultsSettingsBody = {
  type: 'model_settings',
  detail: {
    type: 'session_defaults_changed',
    command_id: '00000000-0000-0000-0000-000000000004',
    prior_defaults_version: '1',
    installed_defaults_version: '2',
    prior_model: {
      kind: 'direct',
      selection_id: '00000000-0000-0000-0000-000000000005',
    },
    installed_model: {
      kind: 'direct',
      selection_id: '00000000-0000-0000-0000-000000000006',
    },
    prior_settings: settingsSnapshot,
    installed_settings: settingsSnapshot,
    caller_override: inheritedSettingsOverlay,
    adjustments: [],
  },
}

function expectTimelineBodyRejected(
  kind: string,
  body: object,
  expectedMessage = 'timeline_detail_page.items[0].body must be one recognized variant',
) {
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

  expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(expectedMessage)
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
            projected_member_index: 0,
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
                evidence: {
                  type: 'physical_attempt',
                  attempt_id: '00000000-0000-0000-0000-000000000005',
                  effect_posture: 'effect_free',
                  state: 'known_failed',
                  cause: 'invalid_arguments',
                },
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
            rationale: {
              text: 'safe read',
              offset_bytes: '0',
              total_bytes: '9',
              continuation: null,
            },
            approval_judge_escalated: false,
          },
          projected_body_bytes: 137,
        },
      ],
      projected_body_bytes: 137,
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
            projected_member_index: 0,
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
            projected_member_index: 0,
            state: {
              type: 'results_projected',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000004',
                tool_name: 'spawn_session',
                arguments: {
                  text: '{}',
                  offset_bytes: '0',
                  total_bytes: '2',
                  continuation: null,
                },
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                evidence: {
                  type: 'physical_attempt',
                  attempt_id: '00000000-0000-0000-0000-000000000005',
                  effect_posture: 'external_effect',
                  state: 'awaiting_child',
                },
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 130,
        },
      ],
      projected_body_bytes: 130,
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
              type: 'superseded',
              generation: '7',
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
          evidence: { type: 'request_only' },
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
          evidence: {
            type: 'physical_attempt',
            attempt_id: '00000000-0000-0000-0000-000000000005',
            effect_posture: 'future',
            state: 'prepared',
          },
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
          evidence: {
            type: 'physical_attempt',
            attempt_id: '00000000-0000-0000-0000-000000000005',
            effect_posture: 'effect_free',
            sandbox_posture: 'future_sandbox',
            state: 'prepared',
          },
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
            terminal_frontier_id: '00000000-0000-0000-0000-00000000000d',
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
      terminal_frontier_id: '00000000-0000-0000-0000-00000000000d',
      attempt_count: '2',
      exhausted: true,
      operator_required: true,
      cause_code: 'ambiguous_operation',
    })
  })

  it('rejects non-parking reconciliation facts', () => {
    const body = {
      type: 'reconciliation',
      turn_id: '00000000-0000-0000-0000-000000000002',
      operation: {
        type: 'model_call',
        model_call_id: '00000000-0000-0000-0000-000000000003',
      },
      terminal_frontier_id: '00000000-0000-0000-0000-00000000000d',
      attempt_count: '2',
      exhausted: true,
      operator_required: true,
      cause_code: 'ambiguous_operation',
    }

    const exhaustedPage = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '13' },
          kind: 'turn_reconciliation_required',
          body: { ...body, exhausted: false },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }
    const operatorPage = {
      ...exhaustedPage,
      items: [{ ...exhaustedPage.items[0], body: { ...body, operator_required: false } }],
    }
    const causePage = {
      ...exhaustedPage,
      items: [{ ...exhaustedPage.items[0], body: { ...body, cause_code: 'future' } }],
    }

    expect(() => decodeWebSessionTimelineDetailPage(exhaustedPage)).toThrow(
      'an exhausted operator-required ambiguous_operation reconciliation',
    )
    expect(() => decodeWebSessionTimelineDetailPage(operatorPage)).toThrow(
      'an exhausted operator-required ambiguous_operation reconciliation',
    )
    expect(() => decodeWebSessionTimelineDetailPage(causePage)).toThrow(
      'an exhausted operator-required ambiguous_operation reconciliation',
    )
  })

  it('rejects impossible tool-attempt evidence', () => {
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
          approval_posture: 'auto',
          approval_judge_escalated: false,
          operator_required: false,
          evidence: { type: 'physical_attempt', state: 'completed' },
        },
      ],
      goal_events: [],
    })
  })

  it('rejects an unknown tool failure cause', () => {
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
          evidence: {
            type: 'physical_attempt',
            attempt_id: '00000000-0000-0000-0000-000000000005',
            effect_posture: 'effect_free',
            state: 'known_failed',
            cause: 'future',
          },
        },
      ],
      goal_events: [],
    })
  })

  it('rejects oversized model-setting adjustments', () => {
    const adjustment = { type: 'fast_mode_disabled' }

    expectTimelineBodyRejected('session_model_settings_changed', {
      ...sessionDefaultsSettingsBody,
      detail: {
        ...sessionDefaultsSettingsBody.detail,
        adjustments: [adjustment, adjustment, adjustment, adjustment],
      },
    })
  })

  it('rejects a missing operator requirement for human approval', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
        state: {
          type: 'proposed',
          frontier_id: '00000000-0000-0000-0000-000000000008',
        },
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            approval_posture: 'human',
            approval_judge_escalated: false,
            operator_required: false,
            evidence: { type: 'request_only' },
          },
        ],
        goal_events: [],
      },
      'operator_required must be equal to approval_judge_escalated || approval_posture === human',
    )
  })

  it('rejects an operator requirement without approval evidence', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
        state: {
          type: 'proposed',
          frontier_id: '00000000-0000-0000-0000-000000000008',
        },
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: true,
            evidence: { type: 'request_only' },
          },
        ],
        goal_events: [],
      },
      'operator_required must be equal to approval_judge_escalated || approval_posture === human',
    )
  })

  it('rejects a mismatched model-settings header', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '13' },
          kind: 'turn_model_settings_resolved',
          body: sessionDefaultsSettingsBody,
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'session_model_settings_changed for session defaults detail',
    )
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
            projected_member_index: 0,
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
                evidence: { type: 'request_only' },
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
      evidence: { type: 'request_only' },
    }
    const secondTool = {
      request_id: '00000000-0000-0000-0000-000000000005',
      tool_name: 'read_file',
      approval_posture: 'auto',
      approval_judge_escalated: false,
      operator_required: false,
      evidence: { type: 'request_only' },
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
      terminal_frontier_id: '00000000-0000-0000-0000-00000000000d',
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

  it('rejects a non-user actor for an escalated approval decision', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '21' },
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

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'a user actor when the approval judge escalated',
    )
  })

  it('accepts a user actor for an escalated approval decision', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '21' },
          kind: 'tool_approval_decided',
          body: {
            type: 'tool_approval_decision',
            turn_id: '00000000-0000-0000-0000-000000000002',
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            decision: 'approve',
            actor: {
              type: 'user',
              command_id: '00000000-0000-0000-0000-000000000005',
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

  function completedArgumentsToolBatchPage(evidence: object, continuationField: string) {
    const address = { event_sequence: '22' }
    return {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address,
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            projected_member_index: 0,
            state: {
              type: 'results_projected',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000004',
                tool_name: 'exec',
                arguments: {
                  text: 'abcd',
                  offset_bytes: '0',
                  total_bytes: '4',
                  continuation: null,
                },
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                evidence,
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 132,
        },
      ],
      projected_body_bytes: 132,
      continuation: {
        type: 'more_body',
        body: {
          address,
          field: continuationField,
          member_index: 0,
          offset_bytes: '0',
        },
      },
    }
  }

  it('accepts a same-member result continuation for a completed attempt', () => {
    const page = completedArgumentsToolBatchPage(
      {
        type: 'physical_attempt',
        attempt_id: '00000000-0000-0000-0000-000000000005',
        effect_posture: 'effect_free',
        state: 'completed',
      },
      'tool_result',
    )

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a same-member result continuation for a request-only member', () => {
    const page = completedArgumentsToolBatchPage({ type: 'request_only' }, 'tool_result')

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow('the excerpt body continuation')
  })

  it('rejects a same-member failure continuation for a completed attempt', () => {
    const page = completedArgumentsToolBatchPage(
      {
        type: 'physical_attempt',
        attempt_id: '00000000-0000-0000-0000-000000000005',
        effect_posture: 'effect_free',
        state: 'completed',
      },
      'tool_failure',
    )

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow('the excerpt body continuation')
  })

  it('rejects a same-member result continuation for a prepared attempt', () => {
    const page = completedArgumentsToolBatchPage(
      {
        type: 'physical_attempt',
        attempt_id: '00000000-0000-0000-0000-000000000005',
        effect_posture: 'effect_free',
        state: 'prepared',
      },
      'tool_result',
    )

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow('the excerpt body continuation')
  })

  function childResultBody(
    outcome: string,
    reason: string,
    provenance: object,
    content: object | null,
  ) {
    return {
      type: 'delegation',
      detail: {
        type: 'child_result',
        relationship_id: '00000000-0000-0000-0000-000000000004',
        child_session_id: '00000000-0000-0000-0000-000000000005',
        outcome,
        reason,
        provenance,
        content,
      },
    }
  }

  const childTurnProvenance = {
    type: 'child_turn',
    session_id: '00000000-0000-0000-0000-000000000005',
    turn_id: '00000000-0000-0000-0000-000000000006',
  }

  const returnedContent = {
    text: 'done',
    offset_bytes: '0',
    total_bytes: '4',
    continuation: null,
  }

  it('accepts a returned child result with completed child-turn provenance', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '23' },
          kind: 'delegation_update',
          body: childResultBody(
            'result_returned',
            'child_completed',
            childTurnProvenance,
            returnedContent,
          ),
          projected_body_bytes: 132,
        },
      ],
      projected_body_bytes: 132,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a returned child result carrying a failure reason', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody(
        'result_returned',
        'child_execution_failed',
        childTurnProvenance,
        returnedContent,
      ),
      'a durable delegation outcome shape',
    )
  })

  it('rejects a returned child result with parent-command provenance', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody(
        'result_returned',
        'child_completed',
        {
          type: 'parent_turn_command',
          session_id: '00000000-0000-0000-0000-000000000001',
          turn_id: '00000000-0000-0000-0000-000000000002',
          command_id: '00000000-0000-0000-0000-000000000007',
        },
        returnedContent,
      ),
      'a durable delegation outcome shape',
    )
  })

  it('rejects a returned child result without content', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody('result_returned', 'child_completed', childTurnProvenance, null),
      'present exactly for a returned child result',
    )
  })

  it('rejects a failed child result carrying content', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody(
        'child_failed',
        'child_execution_failed',
        childTurnProvenance,
        returnedContent,
      ),
      'present exactly for a returned child result',
    )
  })

  it('rejects a non-canonical delegation child session identity', () => {
    expectTimelineBodyRejected('delegation_update', {
      type: 'delegation',
      detail: {
        type: 'child_spawned',
        relationship_id: '00000000-0000-0000-0000-000000000004',
        child_session_id: 'not-a-uuid',
        policy: { type: 'background' },
      },
    })
  })

  it('rejects an empty runner working directory', () => {
    expectTimelineBodyRejected('runner_state_transition', {
      type: 'runner',
      runner_id: '00000000-0000-0000-0000-000000000007',
      placement_revision: '1',
      sandbox_posture: 'sandboxed',
      working_directory: '',
      state: 'pinned',
    })
  })

  it('rejects a NUL-containing runner working directory', () => {
    expectTimelineBodyRejected('runner_state_transition', {
      type: 'runner',
      runner_id: '00000000-0000-0000-0000-000000000007',
      placement_revision: '1',
      sandbox_posture: 'sandboxed',
      working_directory: '/workspace/\u0000/repo',
      state: 'pinned',
    })
  })

  it('rejects an oversized runner working directory', () => {
    expectTimelineBodyRejected('runner_state_transition', {
      type: 'runner',
      runner_id: '00000000-0000-0000-0000-000000000007',
      placement_revision: '1',
      sandbox_posture: 'sandboxed',
      working_directory: `/${'x'.repeat(4096)}`,
      state: 'pinned',
    })
  })

  it('accepts a bounded runner working directory', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '24' },
          kind: 'runner_state_transition',
          body: {
            type: 'runner',
            runner_id: '00000000-0000-0000-0000-000000000007',
            placement_revision: '1',
            sandbox_posture: 'sandboxed',
            working_directory: '/workspace/repo',
            state: 'pinned',
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  function turnResolvedBody(overrides: object) {
    return {
      type: 'model_settings',
      detail: {
        type: 'turn_resolved',
        accepted_input_id: '00000000-0000-0000-0000-000000000009',
        turn_id: '00000000-0000-0000-0000-000000000002',
        defaults_version: '1',
        requested_model: {
          kind: 'direct',
          selection_id: '00000000-0000-0000-0000-00000000000a',
        },
        selected_direct_id: '00000000-0000-0000-0000-00000000000a',
        per_call_override: inheritedSettingsOverlay,
        settings: settingsSnapshot,
        adjusted_from_selection_id: null,
        adjustments: [],
        ...overrides,
      },
    }
  }

  it('accepts a coherent resolved direct selection', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '25' },
          kind: 'turn_model_settings_resolved',
          body: turnResolvedBody({}),
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a resolved direct selection naming another selected identity', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        selected_direct_id: '00000000-0000-0000-0000-00000000000b',
      }),
      'the requested direct selection identity',
    )
  })

  it('rejects a snapshot validated for another selection', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        settings: {
          ...settingsSnapshot,
          precedence: {
            ...settingsSnapshot.precedence,
            session: {
              ...inheritedSettingsOverlay,
              fast_mode: { kind: 'value', value: 'enabled' },
            },
          },
          effective: { fast_mode: 'enabled' },
          fast_mode_source: 'session',
          validated_for_selection_id: '00000000-0000-0000-0000-00000000000b',
        },
      }),
      'the selected direct identity',
    )
  })

  it('rejects adjustments without their prior selection identity', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        adjustments: [{ type: 'fast_mode_disabled' }],
      }),
      'present exactly with recorded adjustments',
    )
  })

  it('rejects a prior adjustment identity equal to the selected identity', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        adjusted_from_selection_id: '00000000-0000-0000-0000-00000000000a',
        adjustments: [{ type: 'fast_mode_disabled' }],
      }),
      'a prior direct selection different from the selected identity',
    )
  })

  it('rejects a tool member alongside a textless goal member', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
        state: {
          type: 'proposed',
          frontier_id: '00000000-0000-0000-0000-000000000008',
        },
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: false,
            evidence: { type: 'request_only' },
          },
        ],
        goal_events: [{ type: 'user_stopped', generation: '1' }],
      },
      'one projected tool or goal member',
    )
  })

  it('accepts a lone textless goal member', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '26' },
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            projected_member_index: 0,
            state: {
              type: 'proposed',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [],
            goal_events: [{ type: 'user_stopped', generation: '1' }],
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a non-successor installed defaults version', () => {
    expectTimelineBodyRejected(
      'session_model_settings_changed',
      {
        ...sessionDefaultsSettingsBody,
        detail: {
          ...sessionDefaultsSettingsBody.detail,
          prior_defaults_version: '1',
          installed_defaults_version: '3',
        },
      },
      'the checked successor of the prior defaults version',
    )
  })

  function sessionMessageBody(recipient: string) {
    return {
      type: 'delegation',
      detail: {
        type: 'session_message',
        relationship_id: '00000000-0000-0000-0000-000000000004',
        message_id: '00000000-0000-0000-0000-000000000008',
        sender_session_id: '00000000-0000-0000-0000-000000000005',
        recipient_session_id: recipient,
        message_ordinal: '1',
        delivery_sequence: '1',
        content: {
          text: 'hi',
          offset_bytes: '0',
          total_bytes: '2',
          continuation: null,
        },
      },
    }
  }

  it('rejects a session message addressed to another session', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      sessionMessageBody('00000000-0000-0000-0000-000000000009'),
      'the enclosing page session',
    )
  })

  it('accepts a session message addressed to the page session', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '27' },
          kind: 'delegation_update',
          body: sessionMessageBody(ambiguousModelCallPage.session_id),
          projected_body_bytes: 130,
        },
      ],
      projected_body_bytes: 130,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a lifecycle body for a reconciliation-required event', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '28' },
          kind: 'turn_reconciliation_required',
          body: {
            type: 'turn_lifecycle',
            turn_id: '00000000-0000-0000-0000-000000000002',
            lifecycle: 'terminalized',
            cause_code: 'reconciliation_required',
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'a terminal turn event for a terminalized lifecycle',
    )
  })

  it('rejects child-turn provenance naming another session', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody(
        'result_returned',
        'child_completed',
        {
          type: 'child_turn',
          session_id: '00000000-0000-0000-0000-000000000009',
          turn_id: '00000000-0000-0000-0000-000000000006',
        },
        returnedContent,
      ),
      "the relationship's child session",
    )
  })

  it('rejects a self-child delegation detail', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      {
        type: 'delegation',
        detail: {
          type: 'child_spawned',
          relationship_id: '00000000-0000-0000-0000-000000000004',
          child_session_id: ambiguousModelCallPage.session_id,
          policy: { type: 'background' },
        },
      },
      'a session other than the relationship parent',
    )
  })

  it('rejects a per-call override differing from the frozen layer', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        per_call_override: {
          ...inheritedSettingsOverlay,
          fast_mode: { kind: 'value', value: 'disabled' },
        },
      }),
      'the frozen per-call settings layer',
    )
  })

  it('rejects a retirement detail for a non-retiring goal event', () => {
    expectTimelineBodyRejected(
      'goal_turn_retired',
      {
        type: 'goal_event',
        turn_id: '00000000-0000-0000-0000-000000000002',
        event: {
          type: 'commissioned',
          generation: '7',
          text: { text: 'goal', offset_bytes: '0', total_bytes: '4' },
        },
      },
      'a retiring goal event',
    )
  })

  it('rejects a no-op session-defaults change', () => {
    expectTimelineBodyRejected(
      'session_model_settings_changed',
      {
        ...sessionDefaultsSettingsBody,
        detail: {
          ...sessionDefaultsSettingsBody.detail,
          installed_model: sessionDefaultsSettingsBody.detail.prior_model,
        },
      },
      'a defaults change that changes the model or settings',
    )
  })

  it('rejects a defaults snapshot validated for another direct model', () => {
    expectTimelineBodyRejected(
      'session_model_settings_changed',
      {
        ...sessionDefaultsSettingsBody,
        detail: {
          ...sessionDefaultsSettingsBody.detail,
          installed_settings: {
            ...settingsSnapshot,
            precedence: {
              ...settingsSnapshot.precedence,
              session: {
                ...inheritedSettingsOverlay,
                fast_mode: { kind: 'value', value: 'enabled' },
              },
            },
            effective: { fast_mode: 'enabled' },
            fast_mode_source: 'session',
            validated_for_selection_id: '00000000-0000-0000-0000-00000000000b',
          },
        },
      },
      'the direct model that validated the snapshot',
    )
  })

  it('rejects an adjustment contradicted by the effective settings', () => {
    expectTimelineBodyRejected(
      'turn_model_settings_resolved',
      turnResolvedBody({
        adjusted_from_selection_id: '00000000-0000-0000-0000-00000000000b',
        adjustments: [{ type: 'fast_mode_disabled' }],
        settings: {
          ...settingsSnapshot,
          precedence: {
            ...settingsSnapshot.precedence,
            session: {
              ...inheritedSettingsOverlay,
              fast_mode: { kind: 'value', value: 'enabled' },
            },
          },
          effective: { fast_mode: 'enabled' },
          fast_mode_source: 'session',
          validated_for_selection_id: '00000000-0000-0000-0000-00000000000a',
        },
      }),
      'a disabled effective fast mode after the adjustment',
    )
  })

  it('rejects a tool member without any projected text field', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
        state: {
          type: 'proposed',
          frontier_id: '00000000-0000-0000-0000-000000000008',
        },
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: false,
            evidence: { type: 'request_only' },
          },
        ],
        goal_events: [],
      },
      'exactly one projected text field',
    )
  })

  it('rejects a delegate approval without its rationale', () => {
    expectTimelineBodyRejected(
      'tool_approval_decided',
      {
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
        approval_judge_escalated: false,
      },
      'the checked rationale a delegate decision always carries',
    )
  })

  it('rejects a zero runner placement revision', () => {
    expectTimelineBodyRejected('runner_state_transition', {
      type: 'runner',
      runner_id: '00000000-0000-0000-0000-000000000007',
      placement_revision: '0',
      sandbox_posture: 'sandboxed',
      state: 'pinned',
    })
  })

  it('rejects a zero goal generation', () => {
    expectTimelineBodyRejected('goal_turn_retired', {
      type: 'goal_event',
      turn_id: '00000000-0000-0000-0000-000000000002',
      event: { type: 'user_stopped', generation: '0' },
    })
  })

  it('rejects a non-UUID tool-batch turn identity', () => {
    expectTimelineBodyRejected('tool_batch_transition', {
      type: 'tool_batch',
      turn_id: 'not-a-uuid',
      producing_model_call_id: '00000000-0000-0000-0000-000000000003',
      projected_member_index: null,
      state: {
        type: 'proposed',
        frontier_id: '00000000-0000-0000-0000-000000000008',
      },
      tools: [],
      goal_events: [],
    })
  })

  it('rejects an oversized multibyte runner working directory', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '29' },
          kind: 'runner_state_transition',
          body: {
            type: 'runner',
            runner_id: '00000000-0000-0000-0000-000000000007',
            placement_revision: '1',
            sandbox_posture: 'sandboxed',
            working_directory: '\u00e9'.repeat(3000),
            state: 'pinned',
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow('at most 4096 UTF-8 bytes')
  })

  it('accepts a repeated-member excerpt continuing at its own index', () => {
    const address = { event_sequence: '30' }
    const continuation = {
      address,
      field: 'tool_arguments',
      member_index: 1,
      offset_bytes: '4',
    }
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address,
          kind: 'tool_batch_transition',
          body: {
            type: 'tool_batch',
            turn_id: '00000000-0000-0000-0000-000000000002',
            producing_model_call_id: '00000000-0000-0000-0000-000000000003',
            projected_member_index: 1,
            state: {
              type: 'proposed',
              frontier_id: '00000000-0000-0000-0000-000000000008',
            },
            tools: [
              {
                request_id: '00000000-0000-0000-0000-000000000005',
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
                evidence: { type: 'request_only' },
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

  it('accepts a lifecycle disposition on the child session page', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '31' },
          kind: 'delegation_update',
          body: {
            type: 'delegation',
            detail: {
              type: 'child_lifecycle_disposition',
              relationship_id: '00000000-0000-0000-0000-000000000004',
              child_session_id: ambiguousModelCallPage.session_id,
              event_ordinal: '2',
              outcome: 'child_stopped',
              reason: 'parent_stopped_with_descendants',
              provenance: {
                type: 'parent_turn_command',
                session_id: '00000000-0000-0000-0000-000000000009',
                turn_id: '00000000-0000-0000-0000-000000000002',
                command_id: '00000000-0000-0000-0000-000000000007',
              },
            },
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  })

  it('rejects a lifecycle disposition with a child-result shape', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      {
        type: 'delegation',
        detail: {
          type: 'child_lifecycle_disposition',
          relationship_id: '00000000-0000-0000-0000-000000000004',
          child_session_id: '00000000-0000-0000-0000-000000000005',
          event_ordinal: '2',
          outcome: 'child_failed',
          reason: 'child_execution_failed',
          provenance: {
            type: 'child_turn',
            session_id: '00000000-0000-0000-0000-000000000005',
            turn_id: '00000000-0000-0000-0000-000000000006',
          },
        },
      },
      'a durable lifecycle disposition shape',
    )
  })

  it('rejects an already-terminal child result outcome', () => {
    expectTimelineBodyRejected(
      'delegation_update',
      childResultBody(
        'already_terminal',
        'parent_stopped_with_descendants',
        {
          type: 'parent_turn_command',
          session_id: '00000000-0000-0000-0000-000000000001',
          turn_id: '00000000-0000-0000-0000-000000000002',
          command_id: '00000000-0000-0000-0000-000000000007',
        },
        null,
      ),
      'a durable delegation outcome shape',
    )
  })

  it('rejects duplicate model-change adjustments', () => {
    expectTimelineBodyRejected(
      'session_model_settings_changed',
      {
        ...sessionDefaultsSettingsBody,
        detail: {
          ...sessionDefaultsSettingsBody.detail,
          adjustments: [{ type: 'fast_mode_disabled' }, { type: 'fast_mode_disabled' }],
        },
      },
      'one ordered adjustment per settings knob',
    )
  })

  it('rejects a per-call layer inside a defaults snapshot', () => {
    expectTimelineBodyRejected(
      'session_model_settings_changed',
      {
        ...sessionDefaultsSettingsBody,
        detail: {
          ...sessionDefaultsSettingsBody.detail,
          installed_settings: {
            ...settingsSnapshot,
            precedence: {
              ...settingsSnapshot.precedence,
              per_call: {
                ...inheritedSettingsOverlay,
                fast_mode: { kind: 'value', value: 'enabled' },
              },
            },
            effective: { fast_mode: 'enabled' },
            fast_mode_source: 'per_call',
            validated_for_selection_id: '00000000-0000-0000-0000-000000000006',
          },
        },
      },
      'an all-inherit per-call layer in a defaults snapshot',
    )
  })

  it('rejects a zero delegation event ordinal', () => {
    expectTimelineBodyRejected('delegation_update', {
      type: 'delegation',
      detail: {
        type: 'child_lifecycle_disposition',
        relationship_id: '00000000-0000-0000-0000-000000000004',
        child_session_id: '00000000-0000-0000-0000-000000000005',
        event_ordinal: '0',
        outcome: 'child_stopped',
        reason: 'parent_stopped_with_descendants',
        provenance: {
          type: 'parent_turn_command',
          session_id: '00000000-0000-0000-0000-000000000001',
          turn_id: '00000000-0000-0000-0000-000000000002',
          command_id: '00000000-0000-0000-0000-000000000007',
        },
      },
    })
  })

  it('rejects a policy actor with a deny decision', () => {
    expectTimelineBodyRejected(
      'tool_approval_decided',
      {
        type: 'tool_approval_decision',
        turn_id: '00000000-0000-0000-0000-000000000002',
        request_id: '00000000-0000-0000-0000-000000000004',
        tool_name: 'exec',
        decision: 'deny',
        actor: { type: 'policy' },
        approval_judge_escalated: false,
      },
      'an automatic approval without a rationale for a policy actor',
    )
  })

  it('rejects a tool-failure total beyond its durable bound', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
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
            evidence: {
              type: 'physical_attempt',
              attempt_id: '00000000-0000-0000-0000-000000000005',
              failure: {
                text: 'boom',
                offset_bytes: '0',
                total_bytes: '18446744073709551615',
                continuation: {
                  address: { event_sequence: '13' },
                  field: 'tool_failure',
                  member_index: 0,
                  offset_bytes: '4',
                },
              },
              effect_posture: 'effect_free',
              state: 'known_failed',
              cause: 'execution_failed',
            },
          },
        ],
        goal_events: [],
      },
      'a declared total within the 4096-byte durable bound',
    )
  })

  it('rejects a non-ambiguous recovery target attempt', () => {
    expectTimelineBodyRejected(
      'tool_batch_transition',
      {
        type: 'tool_batch',
        turn_id: '00000000-0000-0000-0000-000000000002',
        producing_model_call_id: '00000000-0000-0000-0000-000000000003',
        projected_member_index: 0,
        state: {
          type: 'recovery_required',
          tool_attempt_id: '00000000-0000-0000-0000-000000000005',
        },
        tools: [
          {
            request_id: '00000000-0000-0000-0000-000000000004',
            tool_name: 'exec',
            arguments: {
              text: '{}',
              offset_bytes: '0',
              total_bytes: '2',
              continuation: null,
            },
            approval_posture: 'auto',
            approval_judge_escalated: false,
            operator_required: false,
            evidence: {
              type: 'physical_attempt',
              attempt_id: '00000000-0000-0000-0000-000000000005',
              result: {
                text: 'done',
                offset_bytes: '0',
                total_bytes: '4',
                continuation: null,
              },
              effect_posture: 'external_effect',
              state: 'completed',
            },
          },
        ],
        goal_events: [],
      },
      'ambiguous for the recovery target attempt',
    )
  })
})

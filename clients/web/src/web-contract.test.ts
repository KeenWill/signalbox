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

  it('rejects a page containing both a projected tool and goal member', () => {
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '21' },
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
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                evidence: { type: 'request_only' },
              },
            ],
            goal_events: [{ type: 'user_stopped', generation: '1' }],
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'timeline_detail_page.items[0].body must be at most one projected tool or goal member',
    )
  })

  it('rejects a tool member containing arguments and result text', () => {
    const excerpt = { text: '', offset_bytes: '0', total_bytes: '0' }
    const page = {
      session_id: ambiguousModelCallPage.session_id,
      items: [
        {
          address: { event_sequence: '22' },
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
                arguments: excerpt,
                approval_posture: 'auto',
                approval_judge_escalated: false,
                operator_required: false,
                evidence: {
                  type: 'physical_attempt',
                  attempt_id: '00000000-0000-0000-0000-000000000005',
                  state: 'completed',
                  effect_posture: 'effect_free',
                  sandbox_posture: 'sandboxed',
                  result: excerpt,
                },
              },
            ],
            goal_events: [],
          },
          projected_body_bytes: 128,
        },
      ],
      projected_body_bytes: 128,
    }

    expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
      'timeline_detail_page.items[0].body.tools[0] must be at most one projected text field',
    )
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
})

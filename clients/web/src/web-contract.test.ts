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
            state: 'results_projected',
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
      continuation: {
        type: 'more_body',
        body: {
          address: { event_sequence: '9' },
          field: 'tool_failure',
          member_index: 0,
          offset_bytes: '0',
        },
      },
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
            source: 'delegate',
            decider: {
              type: 'delegate',
              model_selection_id: '00000000-0000-0000-0000-000000000005',
              model_call_id: '00000000-0000-0000-0000-000000000006',
            },
            approval_judge_escalated: true,
          },
          projected_body_bytes: 160,
        },
      ],
      projected_body_bytes: 160,
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

  const checkedU64Bodies = [
    {
      name: 'goal generation',
      kind: 'goal_turn_retired',
      bodyForValue: (value: string) => ({
        type: 'goal_event',
        turn_id: '00000000-0000-0000-0000-000000000002',
        event: { generation: value, event_kind: 'retired' },
      }),
    },
    {
      name: 'imported position',
      kind: 'session_created',
      bodyForValue: (value: string) => ({
        type: 'session_created',
        imported_evidence: {
          imported_entry_id: '00000000-0000-0000-0000-000000000008',
          imported_position: value,
        },
      }),
    },
    {
      name: 'compaction through position',
      kind: 'context_compacted',
      bodyForValue: (value: string) => ({
        type: 'context_compaction',
        compaction_id: '00000000-0000-0000-0000-000000000009',
        model_call_id: '00000000-0000-0000-0000-000000000003',
        through_position: value,
        summary_entry_id: '00000000-0000-0000-0000-00000000000a',
        result_frontier_id: '00000000-0000-0000-0000-00000000000b',
        summary: { text: 'summary', offset_bytes: '0', total_bytes: '7' },
      }),
    },
    {
      name: 'reconciliation attempt count',
      kind: 'turn_reconciliation_required',
      bodyForValue: (value: string) => ({
        type: 'reconciliation',
        turn_id: '00000000-0000-0000-0000-000000000002',
        operation_kind: 'tool_attempt',
        operation_id: '00000000-0000-0000-0000-00000000000c',
        attempt_count: value,
        exhausted: true,
        operator_required: true,
        cause_code: 'ambiguous_operation',
      }),
    },
    {
      name: 'runner placement revision',
      kind: 'runner_state_transition',
      bodyForValue: (value: string) => ({
        type: 'runner',
        runner_id: '00000000-0000-0000-0000-000000000007',
        placement_revision: value,
        sandbox_posture: 'sandboxed',
        state: 'pinned',
      }),
    },
  ]

  it.each(checkedU64Bodies)(
    'rejects malformed and out-of-range $name values',
    ({ kind, bodyForValue }) => {
      for (const invalid of ['+1', '01', '18446744073709551616']) {
        const page = {
          session_id: ambiguousModelCallPage.session_id,
          items: [
            {
              address: { event_sequence: '13' },
              kind,
              body: bodyForValue(invalid),
              projected_body_bytes: 128,
            },
          ],
          projected_body_bytes: 128,
        }

        expect(() => decodeWebSessionTimelineDetailPage(page)).toThrow(
          'timeline_detail_page.items[0].body must be one recognized variant',
        )
      }
    },
  )
})

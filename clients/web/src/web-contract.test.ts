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
})

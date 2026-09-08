import { describe, expect, it } from 'vitest'
import {
  detailExcerpt,
  detailItems,
  detailPage,
  detailSessionId,
  resultCursor,
} from '../e2e/session-detail-fixture'
import {
  decodeWebSessionTimelineDetailPage,
  type WebSessionTimelineDetail,
} from './generated/web-contract.mjs'
import { isCompatibleDetailBody } from './SessionItemDetail'

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
})

const overlay = {
  reasoning_level: { kind: 'inherit' },
  fast_mode: { kind: 'inherit' },
  service_tier: { kind: 'inherit' },
} as const
const settings = {
  precedence: { per_call: overlay, session: overlay, profile: overlay, global_default: overlay },
  effective: { reasoning_level: null, fast_mode: 'disabled', service_tier: null },
} as const
const selection = { kind: 'direct', selection_id: detailSessionId } as const
const variants: Array<[WebSessionTimelineDetail['kind'], WebSessionTimelineDetail['body']]> = [
  ['session_created', { type: 'session_created', imported_evidence: null }],
  ['session_state_changed', { type: 'session_state', state: 'parked' }],
  ['session_terminal', { type: 'session_terminal', outcome: 'achieved_declared' }],
  ['command_settled', { type: 'command_settlement', command_id: detailSessionId, rejection: null }],
  [
    'injection_settled',
    {
      type: 'injection_settlement',
      command_id: detailSessionId,
      delivered: true,
      turn_id: detailSessionId,
    },
  ],
  ['session_ownership_changed', { type: 'ownership', transition: 'released' }],
  ['goal_turn_retired', { type: 'event_fact', kind: 'goal_turn_retired' }],
  [
    'session_model_settings_changed',
    {
      type: 'model_settings',
      detail: {
        type: 'session_defaults_changed',
        command_id: detailSessionId,
        prior_defaults_version: '0',
        installed_defaults_version: '1',
        prior_model: { kind: 'direct', selection_id: '00000000-0000-0000-0000-000000000125' },
        installed_model: selection,
        prior_settings: settings,
        installed_settings: settings,
        caller_override: overlay,
        adjustments: [],
      },
    },
  ],
  [
    'turn_model_settings_resolved',
    {
      type: 'model_settings',
      detail: {
        type: 'turn_resolved',
        accepted_input_id: detailSessionId,
        turn_id: detailSessionId,
        defaults_version: '1',
        requested_model: selection,
        selected_direct_id: detailSessionId,
        per_call_override: overlay,
        settings,
        adjustments: [],
      },
    },
  ],
  [
    'goal_changed',
    {
      type: 'goal_event',
      session_id: detailSessionId,
      event: {
        type: 'blocked',
        generation: '1',
        reason: 'finish_check_failed',
        text: detailExcerpt(''),
      },
    },
  ],
  [
    'goal_changed',
    {
      type: 'goal_event',
      session_id: detailSessionId,
      event: { type: 'session_closed', generation: '1', outcome: 'stopped' },
    },
  ],
  [
    'context_compacted',
    {
      type: 'context_compaction',
      compaction_id: detailSessionId,
      model_call_id: detailSessionId,
      through_position: '7',
      summary_entry_id: detailSessionId,
      result_frontier_id: detailSessionId,
      summary: detailExcerpt(''),
    },
  ],
  [
    'turn_reconciliation_required',
    {
      type: 'reconciliation',
      turn_id: detailSessionId,
      operation: { type: 'tool_attempt', tool_attempt_id: detailSessionId },
      terminal_frontier_id: detailSessionId,
    },
  ],
  [
    'runner_state_transition',
    {
      type: 'runner',
      runner_id: detailSessionId,
      placement_revision: '1',
      sandbox_posture: 'sandboxed',
      working_directory: '/workspace',
      state: 'pinned',
    },
  ],
  [
    'delegation_update',
    {
      type: 'delegation',
      detail: {
        type: 'child_lifecycle_disposition',
        relationship_id: detailSessionId,
        child_session_id: detailSessionId,
        event_ordinal: '1',
        outcome: 'child_cancelled',
        reason: 'parent_cancelled_with_descendants',
        provenance: {
          type: 'parent_lifecycle_command',
          session_id: detailSessionId,
          command_id: detailSessionId,
        },
      },
    },
  ],
  [
    'delegation_wake',
    {
      type: 'delegation',
      detail: {
        type: 'message_wake',
        relationship_id: detailSessionId,
        message_id: detailSessionId,
      },
    },
  ],
]

it.each(variants)('decodes and presents the %s body', (kind, body) => {
  const page = detailPage([
    { address: { event_sequence: '1' }, kind, body, projected_body_bytes: 128 },
  ])
  const decoded = decodeWebSessionTimelineDetailPage(page)
  expect(decoded.items[0]?.body).toEqual(body)
  expect(isCompatibleDetailBody(kind, body)).toBe(true)
  expect(isCompatibleDetailBody('input_accepted', body)).toBe(false)
})

it.each(detailItems)('decodes the browser fixture for $kind', (item) => {
  const page = detailPage([item], item.kind === 'tool_batch_transition' ? resultCursor : null)
  expect(decodeWebSessionTimelineDetailPage(page)).toEqual(page)
  expect(isCompatibleDetailBody(item.kind, item.body)).toBe(true)
})

it('rejects a generic event fact for an event with a typed body', () => {
  expect(() =>
    decodeWebSessionTimelineDetailPage(
      detailPage([
        {
          address: { event_sequence: '1' },
          kind: 'tool_batch_transition',
          projected_body_bytes: 128,
          body: { type: 'event_fact', kind: 'tool_batch_transition' },
        },
      ]),
    ),
  ).toThrow()
})

it('rejects mismatched lifecycle and delegation subtypes', () => {
  expect(
    isCompatibleDetailBody('turn_completed', {
      type: 'turn_lifecycle',
      turn_id: detailSessionId,
      lifecycle: 'terminalized',
      cause_code: 'failed',
    }),
  ).toBe(false)
  expect(
    isCompatibleDetailBody('delegation_update', {
      type: 'delegation',
      detail: { type: 'result_wake', relationship_id: detailSessionId },
    }),
  ).toBe(false)
})

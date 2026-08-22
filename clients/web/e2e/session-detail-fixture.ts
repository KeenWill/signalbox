import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from '../src/generated/web-contract.mjs'

export const richSessionId = '00000000-0000-0000-0000-000000000991'
export const richTurnId = '00000000-0000-0000-0000-000000000117'

const excerpt = (text: string) => ({
  text,
  offset_bytes: '0',
  total_bytes: String(new TextEncoder().encode(text).byteLength),
  continuation: null,
})

const richDetails: WebSessionTimelineDetailPage['items'] = [
  {
    address: { event_sequence: '89' },
    kind: 'session_created',
    projected_body_bytes: 200,
    body: {
      type: 'session_created',
      imported_evidence: { imported_entry_id: 'imported-entry-17', imported_position: '42' },
    },
  },
  {
    address: { event_sequence: '90' },
    kind: 'session_model_settings_changed',
    projected_body_bytes: 200,
    body: { type: 'model_settings', turn_id: null, cause_code: 'operator_defaults_replaced' },
  },
  {
    address: { event_sequence: '91' },
    kind: 'input_accepted',
    projected_body_bytes: 200,
    body: {
      type: 'user_input',
      turn_id: richTurnId,
      text: excerpt('Inspect the failed deployment and preserve the evidence trail.'),
      attachments: [
        { blob_id: 'sha256:synthetic-log', length_bytes: '8192', media_type: 'text/plain' },
      ],
    },
  },
  {
    address: { event_sequence: '92' },
    kind: 'model_call_transition',
    projected_body_bytes: 200,
    body: {
      type: 'model_call',
      turn_id: richTurnId,
      model_call_id: 'model-call-ambiguous-7',
      state: { type: 'terminal', disposition: 'ambiguous' },
      model_identity_id: 'openai:gpt-synthetic',
      request_context_items: '14',
      response: excerpt('The provider stream ended after reporting partial progress.'),
      usage: {
        input_tokens: '1804',
        output_tokens: '233',
        cache_creation_input_tokens: null,
        cache_read_input_tokens: '1024',
      },
      cause_code: 'provider_boundary_lost_after_send',
    },
  },
  {
    address: { event_sequence: '93' },
    kind: 'tool_batch_transition',
    projected_body_bytes: 200,
    body: {
      type: 'tool_batch',
      turn_id: richTurnId,
      producing_model_call_id: 'model-call-ambiguous-7',
      state: 'parked_for_operator',
      tools: [
        {
          request_id: 'tool-request-4',
          attempt_id: 'tool-attempt-2',
          tool_name: 'exec_command',
          arguments: excerpt('{"cmd":"deploy --inspect"}'),
          result: excerpt('release marker observed before connection loss'),
          failure: excerpt('terminal acknowledgement unavailable'),
          approval_posture: 'confirm',
          approval_judge_escalated: true,
          operator_required: true,
          effect_posture: 'write_external',
          sandbox_posture: 'unsandboxed_approved',
          state: 'ambiguous',
          cause_code: 'tool_transport_ambiguous',
        },
      ],
      goal_events: [
        {
          generation: '3',
          event_kind: 'blocked',
          reason: 'operator_resolution_needed',
          text: excerpt('Need an operator to classify the external side effect.'),
        },
        {
          generation: '3',
          event_kind: 'resumed',
          reason: 'operator_supplied_evidence',
          text: excerpt('Operator confirmed the deployment marker.'),
        },
        {
          generation: '3',
          event_kind: 'achieved',
          reason: 'evidence_reconciled',
          text: excerpt('Deployment inspection completed with retained evidence.'),
        },
      ],
    },
  },
  {
    address: { event_sequence: '94' },
    kind: 'tool_approval_decided',
    projected_body_bytes: 200,
    body: {
      type: 'tool_approval_decision',
      turn_id: richTurnId,
      request_id: 'tool-request-5',
      tool_name: 'publish_release',
      decision: 'denied',
      source: 'user',
      rationale: excerpt('Denied: the release window has closed.'),
      approval_judge_escalated: true,
    },
  },
  {
    address: { event_sequence: '95' },
    kind: 'goal_turn_retired',
    projected_body_bytes: 200,
    body: {
      type: 'goal_event',
      turn_id: richTurnId,
      event: {
        generation: '3',
        event_kind: 'blocked',
        reason: 'awaiting_operator',
        text: excerpt('Need a durable disposition for the ambiguous call.'),
      },
    },
  },
  {
    address: { event_sequence: '96' },
    kind: 'context_compacted',
    projected_body_bytes: 200,
    body: {
      type: 'context_compaction',
      compaction_id: 'compaction-9',
      model_call_id: 'model-call-compact-9',
      through_position: '88',
      summary_entry_id: 'entry-summary-9',
      result_frontier_id: 'frontier-10',
      summary: excerpt('Earlier investigation evidence compacted through transcript position 88.'),
    },
  },
  {
    address: { event_sequence: '97' },
    kind: 'turn_activated',
    projected_body_bytes: 200,
    body: {
      type: 'turn_lifecycle',
      turn_id: richTurnId,
      lifecycle: 'activated',
      cause_code: 'accepted_input_ready',
    },
  },
  {
    address: { event_sequence: '98' },
    kind: 'turn_reconciliation_required',
    projected_body_bytes: 200,
    body: {
      type: 'reconciliation',
      turn_id: richTurnId,
      operation_kind: 'model_call',
      operation_id: 'model-call-ambiguous-7',
      attempt_count: '3',
      exhausted: true,
      operator_required: true,
      cause_code: 'automatic_reconciliation_exhausted',
    },
  },
  {
    address: { event_sequence: '99' },
    kind: 'runner_state_transition',
    projected_body_bytes: 200,
    body: {
      type: 'runner',
      runner_id: 'runner-synthetic-2',
      placement_revision: '12',
      sandbox_posture: 'sandboxed',
      working_directory: '/workspace/signalbox',
      state: 'parked',
    },
  },
  {
    address: { event_sequence: '100' },
    kind: 'delegation_update',
    projected_body_bytes: 200,
    body: {
      type: 'delegation',
      event_kind: 'evidence_imported',
      relationship_id: 'delegation-6',
      subject_id: 'agent-analysis-2',
      outcome: 'returned',
      reason: 'bounded_child_result',
      content: excerpt('Delegated analysis returned three verified deployment facts.'),
    },
  },
  {
    address: { event_sequence: '101' },
    kind: 'turn_failed',
    projected_body_bytes: 200,
    body: {
      type: 'turn_lifecycle',
      turn_id: richTurnId,
      lifecycle: 'terminalized',
      cause_code: 'parked_for_operator_after_ambiguous_effect',
    },
  },
]

export const richTimelineWindow: WebSessionTimelineWindow = {
  session_id: richSessionId,
  items: richDetails.map(({ address, kind }) => ({
    address,
    kind,
    projected_structured_bytes: 96,
  })),
  projected_structured_bytes: richDetails.length * 96,
  continuation_before: { event_sequence: '89' },
  continuation_after: null,
}

const page = (
  items: WebSessionTimelineDetailPage['items'],
  continuation: WebSessionTimelineDetailPage['continuation'] = null,
): WebSessionTimelineDetailPage => ({
  session_id: richSessionId,
  items,
  projected_body_bytes: items.reduce((total, item) => total + item.projected_body_bytes, 0),
  continuation,
})

export const richItemPage = (address: string): WebSessionTimelineDetailPage =>
  page(richDetails.filter((item) => item.address.event_sequence === address))

export const richTurnPage = (): WebSessionTimelineDetailPage =>
  page(richDetails.filter((item) => 'turn_id' in item.body && item.body.turn_id === richTurnId))

export const richRegionPage = (continued: boolean): WebSessionTimelineDetailPage =>
  continued
    ? page(richDetails.slice(7))
    : page(richDetails.slice(0, 7), {
        type: 'more_at',
        address: { event_sequence: '96' },
      })

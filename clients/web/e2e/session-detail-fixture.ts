import type {
  WebSessionLiveSnapshot,
  WebSessionTimelineDetail,
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from '../src/generated/web-contract.mjs'

export const detailSessionId = '00000000-0000-0000-0000-000000000991'
export const detailTurnId = '00000000-0000-0000-0000-000000000117'
export const detailCallId = '00000000-0000-0000-0000-000000000118'
export const detailRequestId = '00000000-0000-0000-0000-000000000119'
const attemptId = '00000000-0000-0000-0000-000000000120'
const frontierId = '00000000-0000-0000-0000-000000000121'

export const detailExcerpt = (text: string) => ({
  text,
  offset_bytes: '0',
  total_bytes: String(new TextEncoder().encode(text).byteLength),
  continuation: null,
})
const input = detailExcerpt('Inspect the release status and retain the result.')
const argumentsText = detailExcerpt('{"cmd":"release status --json"}')
const result = detailExcerpt('{"release":"ready","checks":"passed"}')
const rationale = detailExcerpt('Publishing needs an operator decision during the release window.')
const response = detailExcerpt('The release checks passed. Publishing remains unapproved.')

const toolEvidence = {
  type: 'physical_attempt',
  attempt_id: attemptId,
  state: 'completed',
  effect_posture: 'effect_free',
  sandbox_posture: 'sandboxed',
  result_present: true,
  failure_present: false,
  cause: null,
  result: null,
  failure: null,
} as const
export const detailItems: WebSessionTimelineDetail[] = [
  {
    address: { event_sequence: '1' },
    kind: 'input_accepted',
    projected_body_bytes: 128 + Number(input.total_bytes),
    body: {
      type: 'user_input',
      turn_id: detailTurnId,
      text: input,
      attachments: [
        { blob_id: `sha256:${'a'.repeat(64)}`, length_bytes: '4', media_type: 'image/png' },
      ],
    },
  },
  {
    address: { event_sequence: '2' },
    kind: 'tool_batch_transition',
    projected_body_bytes: 128 + Number(argumentsText.total_bytes),
    body: {
      type: 'tool_batch',
      turn_id: detailTurnId,
      producing_model_call_id: detailCallId,
      state: { type: 'results_projected', frontier_id: frontierId },
      projected_member_index: 0,
      tools: [
        {
          request_id: detailRequestId,
          tool_name: 'exec_command',
          approval_posture: 'auto',
          approval_judge_escalated: false,
          arguments: argumentsText,
          evidence: toolEvidence,
        },
      ],
      goal_events: [],
    },
  },
  {
    address: { event_sequence: '3' },
    kind: 'tool_approval_decided',
    projected_body_bytes: 128 + Number(rationale.total_bytes),
    body: {
      type: 'tool_approval_decision',
      turn_id: detailTurnId,
      request_id: '00000000-0000-0000-0000-000000000122',
      tool_name: 'publish_release',
      decision: 'deny',
      actor: { type: 'user', command_id: '00000000-0000-0000-0000-000000000123' },
      rationale,
      approval_judge_escalated: true,
    },
  },
  {
    address: { event_sequence: '4' },
    kind: 'model_call_transition',
    projected_body_bytes: 128 + Number(response.total_bytes),
    body: {
      type: 'model_call',
      turn_id: detailTurnId,
      model_call_id: '00000000-0000-0000-0000-000000000124',
      model_identity_id: frontierId,
      state: { type: 'terminal', disposition: 'completed' },
      request_context_items: '7',
      response,
      usage: { input_tokens: '1200', output_tokens: '74' },
    },
  },
  {
    address: { event_sequence: '5' },
    kind: 'turn_completed',
    projected_body_bytes: 128,
    body: {
      type: 'turn_lifecycle',
      turn_id: detailTurnId,
      lifecycle: 'terminalized',
      cause_code: 'completed',
    },
  },
]
export const resultCursor = {
  type: 'more_body',
  body: {
    address: { event_sequence: '2' },
    field: 'tool_result',
    member_index: 0,
    offset_bytes: '0',
  },
} as const
export const detailPage = (
  items: WebSessionTimelineDetailPage['items'],
  continuation: WebSessionTimelineDetailPage['continuation'] = null,
): WebSessionTimelineDetailPage => ({
  session_id: detailSessionId,
  items,
  projected_body_bytes: items.reduce((sum, item) => sum + item.projected_body_bytes, 0),
  continuation,
})
export const toolResultItem = (): WebSessionTimelineDetail => {
  const item = detailItems[1]
  if (!item || item.body.type !== 'tool_batch') throw new Error('tool fixture missing')
  const tool = item.body.tools[0]
  if (!tool) throw new Error('tool member fixture missing')
  return {
    ...item,
    projected_body_bytes: 128 + Number(result.total_bytes),
    body: {
      ...item.body,
      tools: [{ ...tool, arguments: null, evidence: { ...toolEvidence, result } }],
    },
  }
}
export const detailWindow: WebSessionTimelineWindow = {
  session_id: detailSessionId,
  items: detailItems.map(({ address, kind }) => ({
    address,
    kind,
    projected_structured_bytes: 64 + kind.length,
  })),
  projected_structured_bytes: detailItems.reduce((sum, { kind }) => sum + 64 + kind.length, 0),
  continuation_before: null,
  continuation_after: null,
}

export const detailLive: WebSessionLiveSnapshot = {
  session_id: detailSessionId,
  observed_through: '5',
  active: null,
  queued_turn_count: '0',
  queued_turn_ids: [],
  reconciliation: null,
  runner: null,
}

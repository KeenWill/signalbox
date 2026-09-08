import { describe, expect, it } from 'vitest'
import {
  detailExcerpt,
  detailItems,
  detailPage,
  detailSessionId,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import {
  decodeWebSessionTimelineDetailPage,
  type WebSessionTimelineDetail,
  type WebTimelineBodyField,
  type WebTimelineTextExcerpt,
} from '../generated/web-contract.mjs'
import { validateDetailContinuation } from './model'

const argumentsItem = () => {
  const item = structuredClone(detailItems[1])
  if (!item || item.body.type !== 'tool_batch') throw new Error('tool arguments fixture missing')
  return item as WebSessionTimelineDetail & {
    body: Extract<WebSessionTimelineDetail['body'], { type: 'tool_batch' }>
  }
}

describe('typed detail continuation', () => {
  it('accepts the exact member and field requested by the prior page', () => {
    const previous = detailPage([argumentsItem()], resultCursor)
    expect(() => validateDetailContinuation(previous, null)).not.toThrow()
    expect(() =>
      validateDetailContinuation(detailPage([toolResultItem()]), resultCursor, previous),
    ).not.toThrow()
  })

  it('rejects a tool page that drops its advertised terminal payload', () => {
    expect(() => validateDetailContinuation(detailPage([argumentsItem()]), null)).toThrow(
      'terminal payload continuation',
    )
    const skipResult = {
      ...resultCursor,
      body: { ...resultCursor.body, field: 'tool_arguments' as const, member_index: 1 },
    }
    expect(() =>
      validateDetailContinuation(detailPage([argumentsItem()], skipResult), null),
    ).toThrow('terminal payload continuation')
  })

  it('rejects a repeated member returned for a different member cursor', () => {
    const cursor = { ...resultCursor, body: { ...resultCursor.body, member_index: 1 } }
    expect(() => validateDetailContinuation(detailPage([toolResultItem()]), cursor)).toThrow(
      'body continuation',
    )
  })

  it('rejects a changed request identity within the same member', () => {
    const previousItem = argumentsItem()
    const tool = previousItem.body.tools[0]
    if (!tool) throw new Error('tool member missing')
    const changed = {
      ...previousItem,
      body: {
        ...previousItem.body,
        tools: [{ ...tool, request_id: '00000000-0000-0000-0000-000000000126' }],
      },
    }
    expect(() =>
      validateDetailContinuation(
        detailPage([toolResultItem()]),
        resultCursor,
        detailPage([changed], resultCursor),
      ),
    ).toThrow('immutable body facts')
  })

  it.each([
    { attempt_id: '00000000-0000-0000-0000-000000000126' },
    {
      state: 'known_failed',
      result_present: false,
      failure_present: true,
      cause: 'execution_failed',
    },
    { effect_posture: 'external_effect' },
    { sandbox_posture: 'unsandboxed' },
  ] as const)('rejects changed frozen attempt evidence across argument chunks: %j', (changed) => {
    const item = argumentsItem()
    const tool = item.body.tools[0]
    if (!tool || tool.evidence.type !== 'physical_attempt')
      throw new Error('physical attempt fixture missing')
    const cursor = {
      ...resultCursor,
      body: { ...resultCursor.body, field: 'tool_arguments' as const, offset_bytes: '1' },
    }
    const nextCursor = { ...cursor, body: { ...cursor.body, offset_bytes: '2' } }
    const chunk = (
      text: string,
      offset: string,
      next: typeof cursor,
      evidence: typeof tool.evidence,
    ) =>
      decodeWebSessionTimelineDetailPage(
        detailPage(
          [
            {
              ...item,
              projected_body_bytes: 129,
              body: {
                ...item.body,
                tools: [
                  {
                    ...tool,
                    evidence,
                    arguments: {
                      text,
                      offset_bytes: offset,
                      total_bytes: '3',
                      continuation: next.body,
                    },
                  },
                ],
              },
            },
          ],
          next,
        ),
      )
    const previous = chunk('a', '0', cursor, tool.evidence)
    const continued = chunk('b', '1', nextCursor, { ...tool.evidence, ...changed })
    expect(() => validateDetailContinuation(previous, null)).not.toThrow()
    expect(() => validateDetailContinuation(continued, cursor, previous)).toThrow(
      'immutable body facts',
    )
  })

  it.each([false, true])(
    'rejects a changed total with earlier fields held: %s',
    (holdArguments) => {
      const item = toolResultItem()
      if (item.body.type !== 'tool_batch') throw new Error('tool result missing')
      const tool = item.body.tools[0]
      if (!tool || tool.evidence.type !== 'physical_attempt')
        throw new Error('physical result missing')
      const changed = {
        ...item,
        body: {
          ...item.body,
          tools: [
            {
              ...tool,
              evidence: {
                ...tool.evidence,
                result: { ...detailExcerpt('changed'), total_bytes: '999' },
              },
            },
          ],
        },
      }
      expect(() =>
        validateDetailContinuation(
          detailPage([changed]),
          resultCursor,
          detailPage(holdArguments ? [argumentsItem(), item] : [item]),
        ),
      ).toThrow('total byte length')
    },
  )

  it('starts fresh reads at the first member and arguments field', () => {
    expect(() => validateDetailContinuation(detailPage([toolResultItem()]), null)).toThrow(
      'skips a text field',
    )
    const item = argumentsItem()
    const changed = { ...item, body: { ...item.body, projected_member_index: 1 } }
    expect(() => validateDetailContinuation(detailPage([changed]), null)).toThrow('skips a member')
  })
})

const otherId = '00000000-0000-0000-0000-000000000992'
const changedId = '00000000-0000-0000-0000-000000000993'
const continuableBodies: {
  name: string
  kind: WebSessionTimelineDetail['kind']
  field: WebTimelineBodyField
  body: (excerpt: WebTimelineTextExcerpt) => Record<string, unknown>
  changes: [string[], unknown][]
}[] = [
  {
    name: 'user input',
    kind: 'input_accepted',
    field: 'input_text',
    body: (text) => ({
      type: 'user_input',
      turn_id: otherId,
      text,
      attachments: [{ blob_id: `sha256:${'a'.repeat(64)}`, length_bytes: '4' }],
    }),
    changes: [
      [['turn_id'], changedId],
      [['attachments', '0', 'length_bytes'], '5'],
    ],
  },
  {
    name: 'model response',
    kind: 'model_call_transition',
    field: 'model_response',
    body: (response) => ({
      type: 'model_call',
      turn_id: otherId,
      model_call_id: otherId,
      model_identity_id: otherId,
      request_context_items: '1',
      state: { type: 'terminal', disposition: 'completed' },
      response,
      usage: { input_tokens: '2', output_tokens: '1' },
    }),
    changes: [
      [['turn_id'], changedId],
      [['model_call_id'], changedId],
      [['model_identity_id'], changedId],
      [['usage', 'input_tokens'], '3'],
    ],
  },
  {
    name: 'approval',
    kind: 'tool_approval_decided',
    field: 'approval_rationale',
    body: (rationale) => ({
      type: 'tool_approval_decision',
      turn_id: otherId,
      request_id: otherId,
      tool_name: 'exec_command',
      decision: 'approve',
      actor: { type: 'user', command_id: otherId },
      approval_judge_escalated: false,
      rationale,
    }),
    changes: [
      [['request_id'], changedId],
      [['actor', 'command_id'], changedId],
      [['decision'], 'deny'],
    ],
  },
  {
    name: 'goal',
    kind: 'goal_changed',
    field: 'goal_text',
    body: (text) => ({
      type: 'goal_event',
      session_id: detailSessionId,
      event: { type: 'blocked', generation: '1', reason: 'user_input_required', text },
    }),
    changes: [
      [['event', 'generation'], '2'],
      [['event', 'reason'], 'authorization_required'],
    ],
  },
  {
    name: 'compaction',
    kind: 'context_compacted',
    field: 'compaction_summary',
    body: (summary) => ({
      type: 'context_compaction',
      compaction_id: otherId,
      model_call_id: otherId,
      through_position: '1',
      summary_entry_id: otherId,
      result_frontier_id: otherId,
      summary,
    }),
    changes: [
      [['compaction_id'], changedId],
      [['through_position'], '2'],
      [['result_frontier_id'], changedId],
    ],
  },
  {
    name: 'delegation message',
    kind: 'delegation_update',
    field: 'delegation_content',
    body: (content) => ({
      type: 'delegation',
      detail: {
        type: 'session_message',
        relationship_id: otherId,
        message_id: otherId,
        sender_session_id: otherId,
        recipient_session_id: detailSessionId,
        delivery_sequence: '1',
        message_ordinal: '1',
        content,
      },
    }),
    changes: [
      [['detail', 'message_id'], changedId],
      [['detail', 'sender_session_id'], changedId],
    ],
  },
  {
    name: 'delegation result',
    kind: 'delegation_update',
    field: 'delegation_content',
    body: (content) => ({
      type: 'delegation',
      detail: {
        type: 'child_result',
        relationship_id: otherId,
        child_session_id: otherId,
        outcome: 'result_returned',
        reason: 'child_completed',
        provenance: { type: 'child_turn', session_id: otherId, turn_id: otherId },
        content,
      },
    }),
    changes: [
      [['detail', 'relationship_id'], changedId],
      [['detail', 'provenance', 'turn_id'], changedId],
    ],
  },
  {
    name: 'tool goal',
    kind: 'tool_batch_transition',
    field: 'goal_text',
    body: (text) => ({
      type: 'tool_batch',
      turn_id: otherId,
      producing_model_call_id: otherId,
      state: { type: 'results_projected', frontier_id: otherId },
      projected_member_index: 0,
      tools: [],
      goal_events: [{ type: 'blocked', generation: '1', reason: 'user_input_required', text }],
    }),
    changes: [
      [['turn_id'], changedId],
      [['state', 'frontier_id'], changedId],
      [['goal_events', '0', 'generation'], '2'],
      [['goal_events', '0', 'reason'], 'authorization_required'],
    ],
  },
]

it.each(
  continuableBodies.flatMap((testCase) =>
    testCase.changes.map(([path, value]) => ({
      ...testCase,
      path,
      value,
    })),
  ),
)('rejects changed $name facts at $path across decoded chunks', (testCase) => {
  const address = { event_sequence: '1' }
  const cursor = {
    type: 'more_body' as const,
    body: {
      address,
      field: testCase.field,
      member_index: 0,
      offset_bytes: '1',
    },
  }
  const page = (body: Record<string, unknown>, continued: boolean) =>
    decodeWebSessionTimelineDetailPage({
      session_id: detailSessionId,
      projected_body_bytes: 129,
      items: [{ address, kind: testCase.kind, body, projected_body_bytes: 129 }],
      continuation: continued ? cursor : null,
    })
  const previous = page(
    testCase.body({ text: 'a', offset_bytes: '0', total_bytes: '2', continuation: cursor.body }),
    true,
  )
  const nextBody = testCase.body({
    text: 'b',
    offset_bytes: '1',
    total_bytes: '2',
    continuation: null,
  })
  const next = page(nextBody, false)
  expect(() => validateDetailContinuation(next, cursor, previous)).not.toThrow()
  let target = nextBody
  for (const key of testCase.path.slice(0, -1)) target = target[key] as Record<string, unknown>
  const key = testCase.path.at(-1)
  if (!key) throw new Error('fixture mutation path missing')
  target[key] = testCase.value
  const changed = page(nextBody, false)
  expect(() => validateDetailContinuation(changed, cursor, previous)).toThrow(
    'immutable body facts',
  )
})

it('accepts key reordering and omitted optional nulls without changing model facts', () => {
  const item = detailItems[3]
  if (!item || item.body.type !== 'model_call') throw new Error('model fixture missing')
  const cursor = {
    type: 'more_body' as const,
    body: {
      address: item.address,
      field: 'model_response' as const,
      member_index: 0,
      offset_bytes: '1',
    },
  }
  const previous = detailPage(
    [
      {
        ...item,
        body: {
          ...item.body,
          provider_failure_cause: null,
          response: { text: 'a', offset_bytes: '0', total_bytes: '2', continuation: cursor.body },
        },
      },
    ],
    cursor,
  )
  const body = Object.fromEntries(
    Object.entries({
      ...item.body,
      response: { text: 'b', offset_bytes: '1', total_bytes: '2', continuation: null },
    }).reverse(),
  )
  const next = decodeWebSessionTimelineDetailPage({
    ...detailPage([]),
    projected_body_bytes: 129,
    items: [{ ...item, body, projected_body_bytes: 129 }],
  })
  expect(() => validateDetailContinuation(next, cursor, previous)).not.toThrow()
})

it('preserves batch facts while advancing to a different tool member', () => {
  const before = argumentsItem()
  const tool = before.body.tools[0]
  if (!tool) throw new Error('tool fixture missing')
  const cursor = {
    type: 'more_body' as const,
    body: { ...resultCursor.body, field: 'tool_arguments' as const, member_index: 1 },
  }
  const next = {
    ...before,
    body: {
      ...before.body,
      projected_member_index: 1,
      tools: [{ ...tool, request_id: changedId }],
    },
  }
  const result = { ...resultCursor, body: { ...resultCursor.body, member_index: 1 } }
  expect(() =>
    validateDetailContinuation(detailPage([next], result), cursor, detailPage([before], cursor)),
  ).not.toThrow()
  expect(() =>
    validateDetailContinuation(
      detailPage([{ ...next, body: { ...next.body, producing_model_call_id: changedId } }], result),
      cursor,
      detailPage([before], cursor),
    ),
  ).toThrow('immutable body facts')
})

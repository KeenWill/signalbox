import { expect, it } from 'vitest'
import {
  detailCallId,
  detailItems,
  detailTurnId,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import { groupTranscriptTurns, toolContinuationSequence, turnSummaryParts } from './turns'

it('groups final text, user messages, and repeated tool evidence under the durable turn identity', () => {
  const turns = groupTranscriptTurns([
    ...detailItems.slice(0, 2),
    toolResultItem(),
    ...detailItems.slice(2),
  ])
  expect(turns).toHaveLength(1)
  const turn = turns[0]
  expect(turn?.turnId).toBe(detailTurnId)
  expect(turn?.messages.map((item) => item.kind)).toEqual(['input_accepted'])
  expect(turn?.result).toEqual(detailItems[3])
  expect(turn?.tools).toHaveLength(1)
  expect(turn?.tools[0]?.arguments?.text).toContain('release status')
  expect(turn?.tools[0]?.evidence).toMatchObject({
    result: { text: '{"release":"ready","checks":"passed"}' },
  })
})

it('does not call a tool-producing response the final assistant result', () => {
  const response = detailItems[3]
  if (response?.body.type !== 'model_call') throw new Error('model fixture missing')
  const intermediate = { ...response, body: { ...response.body, model_call_id: detailCallId } }
  const turn = groupTranscriptTurns([intermediate, ...detailItems.slice(0, 3)])[0]
  expect(turn?.result).toBeUndefined()
  expect(turn?.tools).toHaveLength(1)
})

it('keeps an unowned retired outcome visible without assigning it to a neighboring turn', () => {
  const turns = groupTranscriptTurns([
    ...detailItems,
    {
      address: { event_sequence: '6' },
      kind: 'goal_turn_retired',
      projected_body_bytes: 128,
      body: { type: 'event_fact', kind: 'goal_turn_retired' },
    },
  ])
  expect(turns).toHaveLength(2)
  expect(turns[1]).toMatchObject({
    turnId: null,
    messages: [],
    result: undefined,
    tools: [],
    outcome: { kind: 'goal_turn_retired' },
  })
})

it('requires a completed turn before presenting a completed model response as final', () => {
  expect(groupTranscriptTurns(detailItems.slice(0, 4))[0]?.result).toBeUndefined()
})

it('preserves steering messages after the tools that precede them', () => {
  const input = detailItems[0]
  if (!input) throw new Error('input fixture missing')
  const turn = groupTranscriptTurns([
    input,
    ...detailItems.slice(1, 3),
    { ...input, address: { event_sequence: '4' } },
    ...detailItems.slice(3),
  ])[0]
  if (!turn) throw new Error('turn fixture missing')
  expect(turnSummaryParts(turn).map((part) => part.kind)).toEqual([
    'message',
    'tools',
    'message',
    'message',
  ])
})

it('retains an unsuccessful turn outcome without a completed assistant response', () => {
  const terminal = detailItems[4]
  if (terminal?.body.type !== 'turn_lifecycle') throw new Error('terminal fixture missing')
  const failure = {
    ...terminal,
    kind: 'turn_failed' as const,
    body: { ...terminal.body, cause_code: 'failed' },
  }
  const turn = groupTranscriptTurns([...detailItems.slice(0, 4), failure])[0]
  expect(turn?.result).toBeUndefined()
  expect(turn?.outcome).toEqual(failure)
})

it('continues merged result text at the event that supplied its excerpt', () => {
  const proposal = detailItems[1]
  const result = toolResultItem()
  if (!proposal || result.body.type !== 'tool_batch') throw new Error('tool fixture missing')
  const tool = result.body.tools[0]
  if (tool?.evidence.type !== 'physical_attempt' || !tool.evidence.result)
    throw new Error('result fixture missing')
  const continued = {
    ...tool,
    evidence: {
      ...tool.evidence,
      result: {
        ...tool.evidence.result,
        continuation: {
          address: { event_sequence: '6' },
          field: 'tool_result' as const,
          member_index: 0,
          offset_bytes: '10',
        },
      },
    },
  }
  const turn = groupTranscriptTurns([
    proposal,
    { ...result, address: { event_sequence: '6' }, body: { ...result.body, tools: [continued] } },
  ])[0]
  if (!turn?.tools[0]) throw new Error('grouped tool fixture missing')
  expect(toolContinuationSequence(turn, turn.tools[0])).toBe('6')
})

it('retains provider failures in event order without treating them as terminal outcomes', () => {
  const model = detailItems[3]
  const terminal = detailItems[4]
  if (model?.body.type !== 'model_call' || terminal?.body.type !== 'turn_lifecycle')
    throw new Error('turn fixture missing')
  const failure = {
    ...model,
    body: {
      ...model.body,
      response: null,
      provider_failure_cause: 'quota_exhausted' as const,
      state: { type: 'terminal' as const, disposition: 'known_failed' as const },
    },
  }
  const failedTurn = groupTranscriptTurns([
    failure,
    { ...terminal, kind: 'turn_failed', body: { ...terminal.body, cause_code: 'failed' } },
  ])[0]
  expect(failedTurn?.warnings).toEqual([failure])
  expect(failedTurn?.outcome?.body).toMatchObject({ cause_code: 'failed' })

  const success = { ...model, address: { event_sequence: '6' } }
  const completed = { ...terminal, address: { event_sequence: '7' } }
  const retriedTurn = groupTranscriptTurns([failure, success, completed])[0]
  if (!retriedTurn) throw new Error('retried turn fixture missing')
  expect(retriedTurn.outcome).toBeUndefined()
  expect(turnSummaryParts(retriedTurn).map((part) => part.kind)).toEqual(['message', 'message'])
  expect(
    turnSummaryParts(retriedTurn).map((part) =>
      part.kind === 'message' ? part.item.address.event_sequence : 'tools',
    ),
  ).toEqual([failure.address.event_sequence, '6'])
})

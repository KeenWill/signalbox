import { expect, it } from 'vitest'
import {
  detailCallId,
  detailItems,
  detailTurnId,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import { groupTranscriptTurns } from './turns'

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

it('does not assign session-level lifecycle noise to a neighboring turn', () => {
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
  expect(turns[1]).toMatchObject({ turnId: null, messages: [], result: undefined, tools: [] })
})

it('requires a completed turn before presenting a completed model response as final', () => {
  expect(groupTranscriptTurns(detailItems.slice(0, 4))[0]?.result).toBeUndefined()
})

it('preserves steering messages after the tools that precede them', async () => {
  const { turnSummaryParts } = await import('./turns')
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

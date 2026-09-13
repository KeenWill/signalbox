import { expect, it } from 'vitest'
import {
  detailCallId,
  detailItems,
  detailTurnId,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import {
  groupTranscriptTurns,
  isVisibleTurnEvent,
  toolContinuations,
  toolEvidenceKey,
  turnSummaryParts,
} from './turns'
import { retriedToolItems } from './turns.fixture'

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

it('retains tool-producing assistant text as a non-final message in event order', () => {
  const response = detailItems[3]
  const tool = detailItems[1]
  if (response?.body.type !== 'model_call' || !tool) throw new Error('model/tool fixture missing')
  const intermediate = {
    ...response,
    address: { event_sequence: '2' },
    body: { ...response.body, model_call_id: detailCallId },
  }
  const turn = groupTranscriptTurns([
    detailItems[0]!,
    intermediate,
    { ...tool, address: { event_sequence: '3' } },
    response,
    detailItems[4]!,
  ])[0]
  if (!turn) throw new Error('turn fixture missing')
  expect(turn.messages).toEqual([detailItems[0], intermediate])
  expect(turn.result).toBe(response)
  expect(
    turnSummaryParts(turn).map((part) =>
      part.kind === 'message' ? part.item.address.event_sequence : 'tools',
    ),
  ).toEqual(['1', '2', 'tools', '4'])
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

it('keeps a completed response visible until its turn closure supplies final status', () => {
  const pending = groupTranscriptTurns(detailItems.slice(0, 4))[0]
  if (!pending) throw new Error('turn fixture missing')
  expect(pending.result).toBeUndefined()
  expect(pending.messages).toEqual([detailItems[0], detailItems[3]])
  expect(
    turnSummaryParts(pending).flatMap((part) =>
      part.kind === 'message' ? [part.item.address.event_sequence] : [],
    ),
  ).toEqual(['1', '4'])

  const completed = groupTranscriptTurns(detailItems)[0]
  expect(completed?.result).toEqual(detailItems[3])
  expect(completed?.messages).toEqual([detailItems[0]])
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

it('retains every continuation from merged tool evidence', () => {
  const proposal = detailItems[1]
  const result = toolResultItem()
  if (proposal?.body.type !== 'tool_batch' || result.body.type !== 'tool_batch')
    throw new Error('tool fixture missing')
  const proposed = proposal.body.tools[0]
  const tool = result.body.tools[0]
  if (!proposed?.arguments || tool?.evidence.type !== 'physical_attempt' || !tool.evidence.result)
    throw new Error('result fixture missing')
  const argumentsContinuation = {
    address: { event_sequence: '2' },
    field: 'tool_arguments' as const,
    member_index: 0,
    offset_bytes: '10',
  }
  const continuedProposal = {
    ...proposal,
    body: {
      ...proposal.body,
      tools: [
        {
          ...proposed,
          arguments: { ...proposed.arguments, continuation: argumentsContinuation },
        },
      ],
    },
  }
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
    continuedProposal,
    { ...result, address: { event_sequence: '6' }, body: { ...result.body, tools: [continued] } },
  ])[0]
  if (!turn?.tools[0]) throw new Error('grouped tool fixture missing')
  expect(toolContinuations(turn.tools[0])).toEqual([
    { field: 'arguments', continuation: argumentsContinuation },
    { field: 'output', continuation: continued.evidence.result?.continuation },
  ])
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

it('keeps an intervening turn input before the earlier turn response', () => {
  const input = detailItems[0]
  if (input?.body.type !== 'user_input') throw new Error('Input fixture missing')
  const turns = groupTranscriptTurns([
    ...detailItems.slice(0, 2),
    {
      ...input,
      address: { event_sequence: '3' },
      body: { ...input.body, turn_id: '00000000-0000-0000-0000-000000000126' },
    },
    ...detailItems.slice(2).map((item) => ({
      ...item,
      address: { event_sequence: String(Number(item.address.event_sequence) + 1) },
    })),
  ])
  expect(turns.map((turn) => turn.events.map((event) => event.address.event_sequence))).toEqual([
    ['1', '2'],
    ['3'],
    ['4', '5', '6'],
  ])
  expect(turns[0]?.turnId).toBe(turns[2]?.turnId)
  expect(turns[0]?.id).not.toBe(turns[2]?.id)
  expect(turns[0]?.result).toBeUndefined()
  expect(turns[2]?.result?.address.event_sequence).toBe('5')
})

it('merges repeated tool evidence into the first interleaved turn segment', () => {
  const proposal = detailItems[1]
  const input = detailItems[0]
  const result = toolResultItem()
  if (proposal?.body.type !== 'tool_batch' || input?.body.type !== 'user_input')
    throw new Error('interleaved tool fixture missing')
  const turns = groupTranscriptTurns([
    proposal,
    {
      ...input,
      address: { event_sequence: '3' },
      body: { ...input.body, turn_id: '00000000-0000-0000-0000-000000000126' },
    },
    { ...result, address: { event_sequence: '4' } },
  ])
  expect(turns.map((turn) => turn.tools.length)).toEqual([1, 0, 0])
  expect(turns[0]?.tools[0]?.evidence).toMatchObject({
    result: { text: '{"release":"ready","checks":"passed"}' },
  })
})

it('retains distinct physical attempts of one request across interleaved segments', () => {
  const turns = groupTranscriptTurns(retriedToolItems())
  expect(turns.map((turn) => turn.tools.length)).toEqual([1, 0, 1])
  const failed = turns[0]?.tools[0]
  const succeeded = turns[2]?.tools[0]
  expect(failed?.request_id).toBe(succeeded?.request_id)
  expect(failed?.evidence).toMatchObject({
    attempt_id: '00000000-0000-0000-0000-000000000120',
    cause: 'crash_lost',
    failure: { text: 'Runner disconnected during the release check.' },
  })
  expect(succeeded?.evidence).toMatchObject({
    attempt_id: '00000000-0000-0000-0000-000000000127',
    result: { text: '{"release":"ready","checks":"passed"}' },
  })
  expect(failed?.arguments?.text).toContain('release status')
  expect(succeeded?.arguments).toEqual(failed?.arguments)
  expect(turns.flatMap(turnSummaryParts).map((part) => part.kind)).toEqual([
    'message',
    'tools',
    'message',
    'tools',
    'message',
  ])
})

it('keeps retained turn segments unchanged when an earlier window shares the turn', () => {
  const input = detailItems[0]
  if (!input) throw new Error('Input fixture missing')
  const retained = detailItems.map((item) => ({
    ...item,
    address: { event_sequence: String(Number(item.address.event_sequence) + 1) },
  }))
  const before = groupTranscriptTurns(retained, new Set(['2']))
  const after = groupTranscriptTurns([input, ...retained], new Set(['1', '2']))
  expect(after.map((turn) => turn.id)).toEqual([`${before[0]?.turnId}:1`, before[0]?.id])
  expect(after[1]).toEqual(before[0])
})

it('keeps continued members of a window-first batch in one turn row', () => {
  const first = detailItems[1]
  if (first?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const tool = first.body.tools[0]
  if (!tool) throw new Error('Tool member missing')
  const second = {
    ...first,
    body: {
      ...first.body,
      projected_member_index: 1,
      tools: [{ ...tool, request_id: '00000000-0000-0000-0000-000000000141' }],
    },
  }
  const before = groupTranscriptTurns([first], new Set(['2']))
  const after = groupTranscriptTurns([first, second], new Set(['2']))
  expect(after).toHaveLength(1)
  expect(after[0]?.id).toBe(before[0]?.id)
  expect(after[0]?.tools.map((tool) => tool.request_id)).toEqual([
    tool.request_id,
    '00000000-0000-0000-0000-000000000141',
  ])
})

it('keeps a retained physical tool in its segment when its proposal is prepended', () => {
  const event = detailItems[1]
  if (event?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const proposal = {
    ...event,
    body: {
      ...event.body,
      tools: event.body.tools.map((tool) => ({
        ...tool,
        evidence: { type: 'request_only' as const },
      })),
    },
  }
  const physical = { ...event, address: { event_sequence: '9' } }
  const initial = groupTranscriptTurns([physical], new Set(['9']))
  const assignments = new Map(
    initial.flatMap((turn) => turn.tools.map((tool) => [toolEvidenceKey(tool), turn.id] as const)),
  )
  const extended = groupTranscriptTurns([proposal, physical], new Set(['2', '9']), assignments)
  expect(extended.map((turn) => turn.tools.length)).toEqual([0, 1])
  expect(extended[1]?.id).toBe(initial[0]?.id)
  expect(extended[1]?.tools).toEqual(initial[0]?.tools)
  expect(extended[0] && isVisibleTurnEvent(extended[0], proposal)).toBe(false)
  const evicted = groupTranscriptTurns([proposal], new Set(['2']), assignments)
  expect(evicted[0]?.tools).toHaveLength(1)
})

it('does not treat goal-only batch members as visible tools in a visible turn', () => {
  const batch = detailItems[1]
  if (batch?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const goal = {
    ...batch,
    body: {
      ...batch.body,
      tools: [],
      goal_events: [
        {
          type: 'user_stopped' as const,
          generation: '1',
          abandoned_actions: null,
          settling_turn_id: null,
        },
      ],
    },
  }
  const turn = groupTranscriptTurns([detailItems[0], goal].filter((item) => item !== undefined))[0]
  if (!turn) throw new Error('Turn fixture missing')
  expect(turn.messages).toHaveLength(1)
  expect(turnSummaryParts(turn).map((part) => part.kind)).toEqual(['message'])
  expect(isVisibleTurnEvent(turn, goal)).toBe(false)
})

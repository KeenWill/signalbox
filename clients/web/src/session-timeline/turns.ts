import type {
  WebSessionTimelineDetail,
  WebTimelineBodyContinuation,
  WebTimelineToolAttempt,
} from '../generated/web-contract.mjs'

export interface TranscriptTurn {
  id: string
  turnId: string | null
  events: WebSessionTimelineDetail[]
  messages: WebSessionTimelineDetail[]
  result: WebSessionTimelineDetail | undefined
  tools: WebTimelineToolAttempt[]
  warnings: WebSessionTimelineDetail[]
  outcome?: WebSessionTimelineDetail
}

export function detailTurnId(item: WebSessionTimelineDetail): string | null {
  const body = item.body
  if ('turn_id' in body) return body.turn_id ?? null
  if (body.type === 'model_settings' && body.detail.type === 'turn_resolved')
    return body.detail.turn_id
  return null
}

export function groupTranscriptTurns(items: readonly WebSessionTimelineDetail[]): TranscriptTurn[] {
  const groups = new Map<string, TranscriptTurn>()
  for (const item of items) {
    const turnId = detailTurnId(item)
    const id = turnId ?? `event-${item.address.event_sequence}`
    let group = groups.get(id)
    if (!group) {
      group = {
        id,
        turnId,
        events: [],
        messages: [],
        result: undefined,
        tools: [],
        warnings: [],
      }
      groups.set(id, group)
    }
    group.events.push(item)
    if (item.body.type === 'model_call' && item.body.provider_failure_cause && !item.body.response)
      group.warnings.push(item)
    if (item.body.type === 'tool_batch') {
      for (const tool of item.body.tools) {
        const index = group.tools.findIndex((known) => known.request_id === tool.request_id)
        const previous = group.tools[index]
        if (!previous) group.tools.push(tool)
        else
          group.tools[index] = {
            ...tool,
            arguments: tool.arguments ?? previous.arguments,
            evidence: tool.evidence.type === 'request_only' ? previous.evidence : tool.evidence,
          }
      }
    }
    if (
      item.body.type === 'reconciliation' ||
      (item.body.type === 'event_fact' &&
        (item.body.kind === 'goal_turn_retired' ||
          item.body.kind === 'automatic_reconciliation_exhausted')) ||
      (item.body.type === 'turn_lifecycle' &&
        item.body.lifecycle === 'terminalized' &&
        item.body.cause_code !== 'completed')
    ) {
      group.outcome = item
    }
    if (item.body.type === 'user_input') group.messages.push(item)
    if (
      item.body.type === 'model_call' &&
      item.body.response &&
      item.body.state.type === 'terminal' &&
      item.body.state.disposition === 'completed'
    )
      group.result = item
  }
  for (const group of groups.values()) {
    for (const event of group.events) {
      const body = event.body
      if (
        body.type === 'model_call' &&
        body.response &&
        group.events.some(
          (candidate) =>
            candidate.body.type === 'tool_batch' &&
            candidate.body.producing_model_call_id === body.model_call_id,
        )
      )
        group.messages.push(event)
    }
    if (
      !group.events.some(
        (event) =>
          event.body.type === 'turn_lifecycle' &&
          event.body.lifecycle === 'terminalized' &&
          event.body.cause_code === 'completed' &&
          BigInt(event.address.event_sequence) >
            BigInt(group.result?.address.event_sequence ?? '0'),
      )
    )
      group.result = undefined
    // A tool-producing model response is intermediate conversation, not the turn's final text.
    if (group.result?.body.type === 'model_call') {
      const callId = group.result.body.model_call_id
      if (
        group.events.some(
          (event) =>
            event.body.type === 'tool_batch' && event.body.producing_model_call_id === callId,
        )
      )
        group.result = undefined
    }
  }
  const segments: TranscriptTurn[] = []
  const assignedTools = new Map<string, Set<string>>()
  for (const item of items) {
    const turnId = detailTurnId(item)
    const group = groups.get(turnId ?? `event-${item.address.event_sequence}`)
    if (!group) continue
    let segment = segments.at(-1)
    if (
      !segment ||
      segment.turnId !== turnId ||
      (turnId === null && segment.events[0]?.address.event_sequence !== item.address.event_sequence)
    ) {
      segment = {
        id: `${group.id}:${item.address.event_sequence}`,
        turnId,
        events: [],
        messages: [],
        result: undefined,
        tools: [],
        warnings: [],
      }
      segments.push(segment)
    }
    segment.events.push(item)
    if (group.messages.includes(item)) segment.messages.push(item)
    if (group.result === item) segment.result = item
    if (group.warnings.includes(item)) segment.warnings.push(item)
    if (group.outcome === item) segment.outcome = item
    if (item.body.type === 'tool_batch') {
      let assigned = assignedTools.get(group.id)
      if (!assigned) {
        assigned = new Set()
        assignedTools.set(group.id, assigned)
      }
      for (const evidence of item.body.tools) {
        if (assigned.has(evidence.request_id)) continue
        assigned.add(evidence.request_id)
        const tool = group.tools.find((known) => known.request_id === evidence.request_id)
        if (tool) segment.tools.push(tool)
      }
    }
  }
  return segments
}

export type TurnSummaryPart =
  | { kind: 'message'; item: WebSessionTimelineDetail }
  | { kind: 'tools'; tools: WebTimelineToolAttempt[] }

export function turnSummaryParts(turn: TranscriptTurn): TurnSummaryPart[] {
  const parts: TurnSummaryPart[] = []
  const seen = new Set<string>()
  for (const item of turn.events) {
    if (turn.messages.includes(item) || item === turn.result || turn.warnings.includes(item))
      parts.push({ kind: 'message', item })
    if (item.body.type !== 'tool_batch') continue
    for (const evidence of item.body.tools) {
      if (seen.has(evidence.request_id)) continue
      seen.add(evidence.request_id)
      const tool = turn.tools.find((tool) => tool.request_id === evidence.request_id)
      if (!tool) continue
      const last = parts.at(-1)
      if (last?.kind === 'tools') last.tools.push(tool)
      else parts.push({ kind: 'tools', tools: [tool] })
    }
  }
  return parts
}

export interface ToolContinuation {
  field: 'arguments' | 'output' | 'failure'
  continuation: WebTimelineBodyContinuation
}

export function toolContinuations(tool: WebTimelineToolAttempt): ToolContinuation[] {
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  const continuations: ToolContinuation[] = []
  if (tool.arguments?.continuation)
    continuations.push({ field: 'arguments', continuation: tool.arguments.continuation })
  if (evidence?.result?.continuation)
    continuations.push({ field: 'output', continuation: evidence.result.continuation })
  if (evidence?.failure?.continuation)
    continuations.push({ field: 'failure', continuation: evidence.failure.continuation })
  return continuations
}

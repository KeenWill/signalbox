import type {
  WebSessionTimelineDetail,
  WebTimelineToolAttempt,
} from '../generated/web-contract.mjs'

export interface TranscriptTurn {
  id: string
  turnId: string | null
  events: WebSessionTimelineDetail[]
  messages: WebSessionTimelineDetail[]
  result: WebSessionTimelineDetail | undefined
  tools: WebTimelineToolAttempt[]
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
      group = { id, turnId, events: [], messages: [], result: undefined, tools: [] }
      groups.set(id, group)
    }
    group.events.push(item)
    if (
      item.body.type === 'reconciliation' ||
      (item.body.type === 'turn_lifecycle' &&
        item.body.lifecycle === 'terminalized' &&
        item.body.cause_code !== 'completed')
    )
      group.outcome = item
    if (item.body.type === 'user_input') group.messages.push(item)
    if (
      item.body.type === 'model_call' &&
      item.body.response &&
      item.body.state.type === 'terminal' &&
      item.body.state.disposition === 'completed'
    )
      group.result = item
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
  }
  for (const group of groups.values()) {
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
  return [...groups.values()]
}

export type TurnSummaryPart =
  | { kind: 'message'; item: WebSessionTimelineDetail }
  | { kind: 'tools'; tools: WebTimelineToolAttempt[] }

export function turnSummaryParts(turn: TranscriptTurn): TurnSummaryPart[] {
  const parts: TurnSummaryPart[] = []
  const seen = new Set<string>()
  for (const item of turn.events) {
    if (item.body.type === 'user_input' || item === turn.result)
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

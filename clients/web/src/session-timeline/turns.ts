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

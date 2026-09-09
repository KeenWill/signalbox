import type { WebSessionTimelineDetail } from '../generated/web-contract.mjs'

export const hasConversationContent = (
  item: WebSessionTimelineDetail,
  preceding: readonly WebSessionTimelineDetail[] = [],
): boolean => {
  const body = item.body
  switch (body.type) {
    case 'user_input':
      return true
    case 'model_call':
      return body.response != null || body.provider_failure_cause != null
    case 'tool_batch':
      return body.tools.some((tool) => {
        if (
          tool.evidence.type === 'physical_attempt' &&
          (tool.evidence.result != null || tool.evidence.failure != null)
        )
          return true
        return (
          tool.arguments != null &&
          !preceding.some(
            (prior) =>
              prior.body.type === 'tool_batch' &&
              prior.body.tools.some(
                (member) => member.request_id === tool.request_id && member.arguments != null,
              ),
          )
        )
      })
    case 'reconciliation':
      return true
    case 'turn_lifecycle':
      return (
        body.lifecycle === 'terminalized' &&
        body.cause_code !== 'completed' &&
        !preceding.some(
          (prior) =>
            prior.body.type === 'turn_lifecycle' &&
            prior.body.lifecycle === 'terminalized' &&
            prior.body.turn_id === body.turn_id &&
            prior.body.cause_code === body.cause_code,
        )
      )
    case 'event_fact':
      return body.kind === 'goal_turn_retired' || body.kind === 'automatic_reconciliation_exhausted'
    default:
      return false
  }
}

export const conversationEntryKey = (item: WebSessionTimelineDetail): string => {
  const body = item.body
  if (body.type !== 'tool_batch') return item.address.event_sequence
  const tool = body.tools[0]
  const physical = tool?.evidence.type === 'physical_attempt' ? tool.evidence : null
  const field = physical?.result ? 'result' : physical?.failure ? 'failure' : 'arguments'
  return `${item.address.event_sequence}:${body.projected_member_index}:${field}`
}

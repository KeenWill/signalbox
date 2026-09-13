import { SessionCostChip } from './session-cost'

export function AttentionCost({ sessionId }: { sessionId: string }) {
  return (
    <span className="attention-cost">
      <span className="attention-cost-label">Cost</span>
      <SessionCostChip sessionId={sessionId} />
    </span>
  )
}

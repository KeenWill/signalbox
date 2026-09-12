import { useQuery } from '@tanstack/react-query'
import type { WebUsageCallPage } from '../generated/web-contract.mjs'
import { type CostTotal, costTotalText, totalCost } from './cost'
import { usageSourceOptions } from './queries'
import './usage.css'

function useSummaryCost(sessionId: string, turnId?: string) {
  const source = useQuery(usageSourceOptions)
  const filters = { sessionId, turnId }
  const query = useQuery({
    queryKey: ['search-usage', 'summary', filters],
    enabled: Boolean(sessionId && source.data),
    queryFn: ({ signal }) => {
      if (!source.data) throw new Error('Usage is not connected')
      return source.data.usageSummary(filters, signal)
    },
    select: (summary) => totalCost(summary.groups, summary.truncated),
  })
  return {
    ...query,
    status: source.isError ? ('error' as const) : query.status,
    error: source.error ?? query.error,
  }
}

export function useSessionCost(sessionId: string) {
  return useSummaryCost(sessionId)
}

export function turnCosts(page: WebUsageCallPage): ReadonlyMap<string, CostTotal> {
  const groups = new Map<string, WebUsageCallPage['calls'][number][]>()
  for (const call of page.calls) {
    if (call.turn_id === null) continue
    const group = groups.get(call.turn_id) ?? []
    group.push(call)
    groups.set(call.turn_id, group)
  }
  return new Map(
    [...groups].map(([turnId, calls]) => [turnId, totalCost(calls, page.continuation !== null)]),
  )
}

export function useTurnCosts(sessionId: string) {
  const source = useQuery(usageSourceOptions)
  const query = useQuery({
    queryKey: ['session-turn-costs', sessionId],
    enabled: Boolean(sessionId && source.data),
    queryFn: ({ signal }) => {
      if (!source.data) throw new Error('Usage is not connected')
      return source.data.usageCalls(
        {
          filters: { sessionId },
          order: 'newest',
          maxItems: source.data.limits.max_usage_call_page_items,
        },
        signal,
      )
    },
    select: turnCosts,
  })
  return {
    ...query,
    status: source.isError ? ('error' as const) : query.status,
    error: source.error ?? query.error,
  }
}

export function CostChip({
  sessionId,
  turnId,
  cost,
  status,
}: {
  sessionId: string
  turnId?: string
  cost?: CostTotal
  status: 'pending' | 'error' | 'success'
}) {
  const query = new URLSearchParams({ session: sessionId })
  if (turnId) query.set('turn', turnId)
  const text =
    status === 'error'
      ? 'Cost unavailable'
      : status === 'pending'
        ? 'Loading cost…'
        : cost
          ? costTotalText(cost)
          : 'Cost not loaded'
  return (
    <a
      className="cost-chip"
      href={`/usage?${query}`}
      title={[text, ...(cost?.rates ?? [])].join(' · ')}
    >
      {text}
    </a>
  )
}

export function SessionCostChip({ sessionId }: { sessionId: string }) {
  const cost = useSessionCost(sessionId)
  return <CostChip sessionId={sessionId} cost={cost.data} status={cost.status} />
}

export function TurnCostChip({ sessionId, turnId }: { sessionId: string; turnId: string }) {
  const cost = useSummaryCost(sessionId, turnId)
  return <CostChip sessionId={sessionId} turnId={turnId} cost={cost.data} status={cost.status} />
}

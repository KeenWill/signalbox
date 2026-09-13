import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useId, useState } from 'react'
import type { WebUsageCallPage } from '../generated/web-contract.mjs'
import { type CostTotal, costTotalText, totalCost } from './cost'
import { usageSourceOptions } from './queries'
import './usage.css'

function useSummaryCost(sessionId: string, enabled: boolean, turnId?: string) {
  const client = useQueryClient()
  const filters = { sessionId, turnId }
  const query = useQuery({
    queryKey: ['search-usage', 'summary', filters],
    enabled,
    queryFn: async ({ signal }) => {
      const source = await client.ensureQueryData(usageSourceOptions)
      return source.usageSummary(filters, signal)
    },
    select: (summary) => totalCost(summary.groups, summary.truncated),
  })
  return query
}

export function useSessionCost(sessionId: string) {
  return useSummaryCost(sessionId, Boolean(sessionId))
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
  const client = useQueryClient()
  const query = useQuery({
    queryKey: ['session-turn-costs', sessionId],
    enabled: Boolean(sessionId),
    queryFn: async ({ signal }) => {
      const source = await client.ensureQueryData(usageSourceOptions)
      return source.usageCalls(
        {
          filters: { sessionId },
          order: 'newest',
          maxItems: source.limits.max_usage_call_page_items,
        },
        signal,
      )
    },
    select: turnCosts,
  })
  return query
}

export function CostChip({
  cost,
  status,
}: {
  cost?: CostTotal
  status: 'pending' | 'error' | 'success'
}) {
  const [expanded, setExpanded] = useState(false)
  const detailsId = useId()
  const text =
    status === 'error'
      ? 'Cost unavailable'
      : status === 'pending'
        ? 'Loading cost…'
        : cost
          ? [
              cost.unpricedModels.length ? 'unpriced' : '',
              cost.rates.length || !cost.unpricedModels.length ? `$${cost.amountUsd}` : '',
              cost.rates.length > 1 ? 'mixed pricing' : '',
              cost.incomplete || cost.unpricedModels.length ? 'partial' : '',
            ]
              .filter(Boolean)
              .join(' · ')
          : 'Cost not loaded'
  if (!cost || status !== 'success') return <span className="cost-chip">{text}</span>
  return (
    <span className="cost-chip">
      <button
        type="button"
        className="cost-chip-toggle"
        aria-expanded={expanded}
        aria-controls={detailsId}
        onClick={() => setExpanded(!expanded)}
      >
        {text}
      </button>
      <span id={detailsId} className="cost-chip-details" hidden={!expanded}>
        {[costTotalText(cost), ...cost.rates].join(' · ')}
      </span>
    </span>
  )
}

export function SessionCostChip({ sessionId }: { sessionId: string }) {
  const cost = useSessionCost(sessionId)
  if (!sessionId) return <CostChip status="success" />
  return <CostChip cost={cost.data} status={cost.status} />
}

export function TurnCostChip({ sessionId, turnId }: { sessionId: string; turnId: string }) {
  const cost = useSummaryCost(sessionId, Boolean(sessionId && turnId), turnId)
  if (!sessionId || !turnId) return <CostChip status="success" />
  return <CostChip cost={cost.data} status={cost.status} />
}

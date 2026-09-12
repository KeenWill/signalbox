import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import '../app.css'
import { SearchUsageScenarioSource } from './scenario'
import {
  CostChip,
  SessionCostChip,
  TurnCostChip,
  useSessionCost,
  useTurnCosts,
} from './session-cost'
import { UsageContent, UsageSurface } from './UsageSurface'

function CostPreview({ sessionId, turnId }: { sessionId: string; turnId: string }) {
  const turns = useTurnCosts(sessionId)
  const session = useSessionCost(sessionId)
  return (
    <>
      <button type="button" onClick={() => void session.refetch()}>
        Refresh costs
      </button>
      <section aria-label="Session cost">
        <SessionCostChip sessionId={sessionId} />
      </section>
      <section aria-label="Turn cost">
        <TurnCostChip sessionId={sessionId} turnId={turnId} />
      </section>
      <section aria-label="Recent turn costs">
        {[...(turns.data ?? [])].map(([id, cost]) => (
          <CostChip key={id} sessionId={sessionId} turnId={id} cost={cost} status={turns.status} />
        ))}
      </section>
    </>
  )
}

if (import.meta.env.DEV) {
  const root = document.getElementById('root')
  const query = new URLSearchParams(window.location.search)
  if (root)
    createRoot(root).render(
      <QueryClientProvider
        client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
      >
        {query.get('preview') === 'cost' ? (
          <CostPreview sessionId={query.get('session') ?? ''} turnId={query.get('turn') ?? ''} />
        ) : query.has('http') ? (
          <UsageSurface />
        ) : (
          <UsageContent source={new SearchUsageScenarioSource()} />
        )}
      </QueryClientProvider>,
    )
}

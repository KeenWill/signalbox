import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  parseSearchWith,
  RouterProvider,
  stringifySearchWith,
} from '@tanstack/react-router'
import { createRoot } from 'react-dom/client'
import '../app.css'
import { readProductRouteState } from '../product'
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
          <CostChip key={id} cost={cost} status={turns.status} />
        ))}
      </section>
    </>
  )
}

const source = new SearchUsageScenarioSource()
function Preview() {
  const query = new URLSearchParams(window.location.search)
  return query.get('preview') === 'cost' ? (
    <CostPreview sessionId={query.get('session') ?? ''} turnId={query.get('turn') ?? ''} />
  ) : query.has('http') ? (
    <UsageSurface />
  ) : (
    <UsageContent source={source} />
  )
}
const rootRoute = createRootRoute()
const route = createRoute({
  getParentRoute: () => rootRoute,
  path: '/src/search-usage/preview.html',
  validateSearch: readProductRouteState,
  component: Preview,
})
const router = createRouter({
  routeTree: rootRoute.addChildren([route]),
  parseSearch: parseSearchWith((value) => value),
  stringifySearch: stringifySearchWith(String),
})
if (import.meta.env.DEV) {
  const root = document.getElementById('root')
  if (root)
    createRoot(root).render(
      <QueryClientProvider
        client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
      >
        <RouterProvider router={router} />
      </QueryClientProvider>,
    )
}

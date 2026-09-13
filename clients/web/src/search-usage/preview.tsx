import { QueryClient, QueryClientProvider, useQuery } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  parseSearchWith,
  RouterProvider,
  stringifySearchWith,
  useLocation,
  useNavigate,
} from '@tanstack/react-router'
import { createRoot } from 'react-dom/client'
import '../app.css'
import '../catalog.css'
import { useState } from 'react'
import { readProductRouteState } from '../product'
import { defaultSearchUsageRouteState, SearchUsageWorkbench } from '../SearchUsage'
import { AttentionCost } from './AttentionCost'
import { usageSourceOptions } from './queries'
import { SEARCH_USAGE_SCENARIO_SESSION_ID, SearchUsageScenarioSource } from './scenario'
import {
  CostChip,
  SessionCostChip,
  TurnCostChip,
  useSessionCost,
  useTurnCosts,
} from './session-cost'
import { UsageContent } from './UsageSurface'

function CostPreview({ sessionId, turnId }: { sessionId: string; turnId: string }) {
  const [previewOpen, setPreviewOpen] = useState(false)
  const turns = useTurnCosts(sessionId)
  const session = useSessionCost(sessionId)
  return (
    <>
      <section aria-label="Attention row" className="attention-list" style={{ maxWidth: 520 }}>
        <ol>
          <li className="attention-cost-row">
            <a
              className="attention-session-link"
              aria-label="Example session"
              href={`/sessions?session=${sessionId}&workspace=true`}
            >
              <span className="attention-rail" aria-hidden="true" />
              <span className="attention-identity">
                <strong>Example session</strong>
                <code>{sessionId}</code>
              </span>
              <span className="attention-obligation">Needs approval</span>
              <time>Today, 12:30</time>
              <span aria-hidden="true">→</span>
            </a>
            <button
              className="attention-preview"
              type="button"
              aria-label="Preview example session"
              aria-pressed={previewOpen}
              onClick={() => setPreviewOpen(!previewOpen)}
            >
              Preview
            </button>
            <AttentionCost sessionId={sessionId} />
          </li>
        </ol>
      </section>

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
function HttpUsagePreview() {
  const source = useQuery(usageSourceOptions)
  if (source.isError) return <p role="alert">Usage could not load.</p>
  if (!source.data) return <p role="status">Loading usage…</p>
  return <UsageContent source={source.data} authority="http" />
}
function Preview() {
  const search = useLocation({ select: (location) => location.searchStr })
  const navigate = useNavigate()
  const query = new URLSearchParams(search)
  if (query.get('preview') === 'attention-costs')
    return (
      <ol aria-label="Attention cost rows">
        {[SEARCH_USAGE_SCENARIO_SESSION_ID, '00000000-0000-0000-0000-000000000995'].map(
          (sessionId) => (
            <li key={sessionId}>
              <AttentionCost sessionId={sessionId} />
            </li>
          ),
        )}
      </ol>
    )
  if (query.get('preview') === 'cost')
    return <CostPreview sessionId={query.get('session') ?? ''} turnId={query.get('turn') ?? ''} />
  if (query.has('http')) return <HttpUsagePreview />
  if (query.has('workbench'))
    return (
      <div className="usage-product">
        <button
          type="button"
          onClick={() =>
            void navigate({
              to: '.',
              search: (previous) => ({ ...previous, http: 'true' }),
            })
          }
        >
          Load server usage
        </button>
        <SearchUsageWorkbench
          source={source}
          currentSessionId={SEARCH_USAGE_SCENARIO_SESSION_ID}
          route={{ ...defaultSearchUsageRouteState, view: 'usage', usageSession: 'all' }}
          onRouteChange={() => undefined}
          onReveal={async () => undefined}
        />
      </div>
    )
  return <UsageContent source={source} authority="scenario" />
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

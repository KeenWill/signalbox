import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
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
import { readProductRouteState } from '../product'
import { defaultSearchUsageRouteState, SearchUsageWorkbench } from '../SearchUsage'
import { SEARCH_USAGE_SCENARIO_SESSION_ID, SearchUsageScenarioSource } from './scenario'
import { UsageContent, UsageSurface } from './UsageSurface'

const source = new SearchUsageScenarioSource()
function Preview() {
  const search = useLocation({ select: (location) => location.searchStr })
  const navigate = useNavigate()
  const query = new URLSearchParams(search)
  if (query.has('http')) return <UsageSurface />
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
